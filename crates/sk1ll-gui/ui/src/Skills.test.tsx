// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { SidebarProvider } from '@/components/ui/sidebar';
import { TooltipProvider } from '@/components/ui/tooltip';
import { Skills } from './Skills';
import { AppSidebar } from './App';
import { api } from './api';
import type { ContentEntry, ContentType, StatusView } from './types';

vi.mock('./api', () => ({ api: { skills: vi.fn(), skillTree: vi.fn(), skillFile: vi.fn(), editors: vi.fn(),
  fork: vi.fn(), adoptUnmanaged: vi.fn(), openInEditor: vi.fn() }, errorMessage: (error: unknown) => String(error) }));
const entry = (name: string, type: ContentType, extra: Partial<ContentEntry> = {}): ContentEntry => ({
  name, item: type === 'unmanaged' ? null : `${type === 'fragment' ? 'fragment' : 'skill'}:${name}`, type,
  description: `About ${name}`, draft: null, in_baseline: type !== 'unmanaged', path: null,
  enabled_harnesses: ['claude'], clashes_with_repo: false, variants: type === 'unmanaged' ? [
    { hash: 'sha256:one', description: 'First variant', locations: [{ harness: 'claude', path: '/claude/notes' }] },
    { hash: 'sha256:two', description: 'Second variant', locations: [{ harness: 'codex', path: '/codex/notes' }] },
  ] : [], ...extra,
});
const entries = [entry('writing', 'personal'), entry('pdf', 'import'), entry('style', 'fragment'), entry('notes', 'unmanaged')];
const changed = vi.fn(), reviewDraft = vi.fn(), assignment = vi.fn();
const props = { connection: 'connected' as const, entries, revision: 0, changed, reviewDraft, assignment };
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
  await screen.findByRole('button', { name: /Fork to edit|Adopt to edit|Open in VS Code/ });
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
  expect(screen.getAllByRole('row')[1].textContent).toContain('style');
  fireEvent.keyDown(screen.getByRole('row', { name: 'View Import pdf' }), { key: 'Enter' });
  expect(await screen.findByRole('button', { name: 'Fork to edit' })).toBeTruthy();
});
it('places Skills first in This Machine and shows the Unmanaged count badge', () => {
  const status = { machine_id: 'test', report: { drafts: [], conflicts: [] } } as unknown as StatusView;
  render(<TooltipProvider><SidebarProvider><AppSidebar screen="Skills" setScreen={vi.fn()} status={status} unmanagedCount={2} /></SidebarProvider></TooltipProvider>);
  expect(screen.getByLabelText('2 Unmanaged skills').textContent).toBe('2');
  const links = screen.getAllByRole('button').filter(button => ['Skills', 'Draft', 'Conflicts', 'Imports'].includes(button.textContent ?? ''));
  expect(links[0].textContent).toBe('Skills');
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
  fireEvent.click(screen.getByRole('button', { name: 'Adopt to edit' }));
  expect(screen.getByText(/original is kept in recovery/)).toBeTruthy(); expect(screen.getByText(/selected variant from codex/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Adopt and open' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledWith('code', 'skill:notes'));
  expect(api.adoptUnmanaged).toHaveBeenCalledWith({ name: 'notes', harness: 'codex' });
  expect(vi.mocked(api.adoptUnmanaged).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(api.openInEditor).mock.invocationCallOrder[0]);
});
it('offers plain Adopt without opening an editor', async () => {
  mount(); await select('notes', 'Unmanaged'); fireEvent.click(screen.getByRole('button', { name: 'Adopt' }));
  fireEvent.click(screen.getByRole('button', { name: 'Adopt into Draft' }));
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
  const header = screen.getByRole('region', { name: 'Frontmatter' });
  expect(within(header).getByText('Read PDFs')).toBeTruthy(); expect(within(header).getByText('["claude","codex"]')).toBeTruthy();
  expect(document.querySelector('script')).toBeNull(); fireEvent.click(screen.getByRole('button', { name: 'Raw' }));
  expect(screen.queryByRole('heading', { name: 'Guide' })).toBeNull(); expect(screen.getByText(/Use \*\*bold\*\* text/).tagName).toBe('PRE');
  expect(screen.getByRole('region', { name: 'Frontmatter' })).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Rendered' }));
  expect(screen.getByRole('heading', { name: 'Guide' })).toBeTruthy();
});
it('browses a Fragment single file and keeps deleted items read-only', async () => {
  vi.mocked(api.skillTree).mockResolvedValue([{ path: 'style.md', kind: 'file', size: 100 }]);
  vi.mocked(api.skillFile).mockResolvedValue({ path: 'style.md', text: '# Style', binary: false, too_large: false, size: 100 });
  mount([entry('style', 'fragment', { draft: 'deleted' })]); fireEvent.click(screen.getByRole('row', { name: 'View Fragment style' }));
  expect(await screen.findByRole('heading', { name: 'Style' })).toBeTruthy(); expect(api.skillFile).toHaveBeenCalledWith({ item: 'fragment:style', path: 'style.md' });
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
  fireEvent.click(screen.getByRole('button', { name: 'Adopt to edit' })); fireEvent.click(screen.getByRole('button', { name: 'Adopt and open' }));
  await screen.findAllByText('Cannot Adopt'); expect(api.openInEditor).not.toHaveBeenCalled(); cleanup();
  mount([entry('notes', 'unmanaged', { clashes_with_repo: true })]); await select('notes', 'Unmanaged');
  expect((screen.getByRole('button', { name: 'Adopt to edit' }) as HTMLButtonElement).disabled).toBe(true);
});
it('Harness chips navigate to the Assignment matrix', () => {
  mount(); fireEvent.click(screen.getByRole('button', { name: 'Assignment for writing in claude' })); expect(assignment).toHaveBeenCalledOnce(); expect(screen.queryByRole('dialog')).toBeNull();
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
  fireEvent.click(screen.getByRole('button', { name: 'docs/example.md' }));
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
  for (const name of ['Adopt', 'Adopt to edit', 'Choose editor']) {
    expect(screen.getByRole('button', { name }).hasAttribute('disabled')).toBe(true);
    fireEvent.click(screen.getByRole('button', { name }));
  }
  const trigger = screen.getByRole('button', { name: 'Adopt' }).parentElement!;
  fireEvent.pointerEnter(trigger, { pointerType: 'mouse' });
  fireEvent.mouseMove(trigger);
  fireEvent.mouseEnter(trigger);
  expect(await screen.findByText('Needs a connection to the Hub')).toBeTruthy();
  expect(api.adoptUnmanaged).not.toHaveBeenCalled();
});
it('disables a pending adoption confirmation when the Hub disconnects', async () => {
  const view = render(<TooltipProvider><Skills {...props} /></TooltipProvider>);
  await select('notes', 'Unmanaged');
  fireEvent.click(screen.getByRole('button', { name: 'Adopt' }));
  view.rerender(<TooltipProvider><Skills {...props} connection="offline" /></TooltipProvider>);
  expect(screen.getByRole('button', { name: 'Adopt into Draft' }).hasAttribute('disabled')).toBe(true);
});
