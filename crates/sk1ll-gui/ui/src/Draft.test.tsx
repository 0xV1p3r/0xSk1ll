// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Draft } from './Draft';
import { Confirm } from './shared';
import { api } from './api';
import type { DraftEntry, StatusView } from './types';

vi.mock('./api', () => ({ api: {
  draft: vi.fn(), diff: vi.fn(), commitMessage: vi.fn(), commit: vi.fn(), restore: vi.fn(), pull: vi.fn(), push: vi.fn(),
}, errorMessage: (error: unknown) => String(error) }));
const status = { report: { blocked: false, clashes: [], unpushed: [] } } as unknown as StatusView;
const entry = (item: string): DraftEntry => ({ item, change: 'added', paths: [`${item}/SKILL.md`], fingerprint: 'initial' });
let entries: DraftEntry[];
let diffs: Record<string, string>;
const changed = vi.fn();
beforeEach(() => {
  vi.resetAllMocks();
  entries = [entry('skill:reviewed')]; diffs = { 'skill:reviewed': '+first version' };
  vi.mocked(api.draft).mockImplementation(async () => structuredClone(entries));
  vi.mocked(api.diff).mockImplementation(async item => ({ item, diff: diffs[item] ?? '+new' }));
  vi.mocked(api.commitMessage).mockResolvedValue({ items: ['skill:reviewed'], message: 'Commit reviewed' });
  vi.mocked(api.restore).mockResolvedValue({ discarded: ['skill:reviewed'] });
  vi.mocked(api.commit).mockResolvedValue({ sha: 'abc', items: ['skill:reviewed'], message: 'done', pushed: true, push_error: null });
});
afterEach(cleanup);
const mount = () => render(<Draft revision={0} status={status} changed={changed} />);
const refresh = async (view: ReturnType<typeof mount>, revision: number) => {
  view.rerender(<Draft revision={revision} status={status} changed={changed} />);
  await waitFor(() => expect(api.draft).toHaveBeenCalledTimes(revision + 1));
};
const button = (root: HTMLElement, name: string) => within(root).getByRole('button', { name }) as HTMLButtonElement;

describe('Draft confirmations', () => {
  it('freezes Restore all items and requires review when a new item arrives', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('button', { name: 'Restore all' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/Return skill:reviewed/)).toBeTruthy();
    entries.push(entry('skill:new'));
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Restore all').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:new.*added/)).toBeTruthy();
    expect(api.restore).not.toHaveBeenCalled();
    fireEvent.click(button(dialog, 'Review updated Draft'));
    await waitFor(() => expect(button(dialog, 'Restore all').disabled).toBe(false));
    fireEvent.click(button(dialog, 'Restore all'));
    await waitFor(() => expect(api.restore).toHaveBeenCalledWith({ items: ['skill:reviewed'], all: false }));
  });
  it('invalidates Restore selected when content changes at the same paths', async () => {
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
    fireEvent.click(screen.getByRole('button', { name: 'Restore selected' }));
    const dialog = await screen.findByRole('dialog');
    diffs['skill:reviewed'] = '+second version';
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Restore selected').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
  });
  it('revalidates Commit content and missing items and keeps the edited message on review', async () => {
    entries.push(entry('rule:gone'));
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Clear checkboxes' });
    fireEvent.click(screen.getByRole('button', { name: 'Commit (2)' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByRole('textbox'), { target: { value: 'My message' } });
    diffs['skill:reviewed'] = '+edited again'; entries = [entry('skill:reviewed')];
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Commit').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
    expect(within(dialog).getByText(/rule:gone.*no longer/)).toBeTruthy();
    fireEvent.click(button(dialog, 'Review updated Draft'));
    fireEvent.click(button(dialog, 'Commit'));
    await waitFor(() => expect(api.commit).toHaveBeenCalledWith({ items: ['skill:reviewed'], message: 'My message' }));
  });
  it('invalidates review for binary edits whose textual diff is unchanged', async () => {
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
    fireEvent.click(screen.getByRole('button', { name: 'Commit (1)' }));
    const dialog = await screen.findByRole('dialog');
    entries[0].fingerprint = 'binary-content-changed';
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Commit').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
    expect(api.commit).not.toHaveBeenCalled();
  });
  it('cannot reconfirm Commit when all selected items disappeared', async () => {
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
    fireEvent.click(screen.getByRole('button', { name: 'Commit (1)' }));
    const dialog = await screen.findByRole('dialog');
    entries = [];
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Commit').disabled).toBe(true));
    fireEvent.click(button(dialog, 'Review updated Draft'));
    expect(button(dialog, 'Commit').disabled).toBe(true);
    expect(api.commit).not.toHaveBeenCalled();
  });
  it('keeps review invalidated even if a later refresh restores the original content', async () => {
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
    fireEvent.click(screen.getByRole('button', { name: 'Restore selected' }));
    const dialog = await screen.findByRole('dialog');
    diffs['skill:reviewed'] = '+temporary edit';
    await refresh(view, 1);
    await within(dialog).findByText(/skill:reviewed.*changed/);
    diffs['skill:reviewed'] = '+first version';
    await refresh(view, 2);
    expect(button(dialog, 'Restore selected').disabled).toBe(true);
  });
  it('blocks confirmation and shows errors when a Draft refresh fails', async () => {
    const view = mount();
    await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
    fireEvent.click(screen.getByRole('button', { name: 'Commit (1)' }));
    const dialog = await screen.findByRole('dialog');
    vi.mocked(api.draft).mockRejectedValueOnce(new Error('Draft refresh failed'));
    await refresh(view, 1);
    await within(dialog).findByText(/Draft refresh failed/);
    expect(button(dialog, 'Commit').disabled).toBe(true);
  });
  it('honours disabled in an already-open shared confirmation footer', async () => {
    const action = vi.fn();
    const view = render(<Confirm label="Remove" title="Remove?" description="Review" action={action} onDone={changed} />);
    fireEvent.click(screen.getByRole('button', { name: 'Remove' }));
    const dialog = await screen.findByRole('dialog');
    view.rerender(<Confirm label="Remove" title="Remove?" description="Review" action={action} onDone={changed} disabled />);
    expect(button(dialog, 'Remove').disabled).toBe(true);
    fireEvent.click(button(dialog, 'Remove'));
    expect(action).not.toHaveBeenCalled();
  });
});

it.each(['Restore all', 'Restore selected'])('returns focus to %s after Cancel and Escape', async label => {
  mount();
  await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
  const trigger = screen.getByRole('button', { name: label });
  for (const close of ['Cancel', 'Escape']) {
    trigger.focus(); fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    if (close === 'Cancel') fireEvent.click(button(dialog, 'Cancel'));
    else fireEvent.keyDown(dialog, { key: 'Escape' });
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  }
});

it('returns focus to Commit after Cancel and Escape and to its completion notice after success', async () => {
  mount();
  await screen.findByRole('checkbox', { name: 'Select skill:reviewed' });
  const trigger = screen.getByRole('button', { name: 'Commit (1)' });
  for (const close of ['Cancel', 'Escape', 'Commit']) {
    trigger.focus(); fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    if (close === 'Escape') fireEvent.keyDown(dialog, { key: 'Escape' });
    else fireEvent.click(button(dialog, close));
    if (close === 'Commit') {
      await screen.findByText('Commit complete');
      await waitFor(() => expect(screen.getByText('Commit complete').closest('[tabindex="-1"]')?.contains(document.activeElement)).toBe(true));
      expect(trigger.hasAttribute('disabled')).toBe(true);
    } else await waitFor(() => expect(document.activeElement).toBe(trigger));
  }
});

it('counts added and removed lines without headers and shows friendly names', async () => {
  diffs['skill:reviewed'] = '+++ b/file\n--- a/file\n+one\n+two\n-old'; mount();
  expect(await screen.findByLabelText('+2 / −1')).toBeTruthy(); expect(screen.getByText('reviewed')).toBeTruthy();
});
it('pushes unpushed items', async () => {
  vi.mocked(api.push).mockResolvedValue({ pushed: true });
  render(<Draft revision={0} status={{ ...status, report: { ...status.report, unpushed: ['skill:reviewed'] } }} changed={changed} />);
  expect(screen.getByText('1 unpushed')).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Push now' }));
  await waitFor(() => expect(api.push).toHaveBeenCalledOnce()); await waitFor(() => expect(changed).toHaveBeenCalledOnce());
});

it('selects arriving items until selection is changed by hand', async () => {
  const view = mount();
  expect((await screen.findByRole('button', { name: 'Commit (1)' })).hasAttribute('disabled')).toBe(false);
  entries.push(entry('skill:new'));
  await refresh(view, 1);
  await screen.findByRole('button', { name: 'Commit (2)' });
  fireEvent.click(screen.getByRole('checkbox', { name: 'Select skill:new' }));
  entries.push(entry('rule:later'));
  await refresh(view, 2);
  await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Select rule:later' }).getAttribute('aria-checked')).toBe('false'));
  expect(screen.getByRole('button', { name: 'Commit (1)' })).toBeTruthy();
});
it('expands all diffs in the list and confirmation', async () => {
  mount();
  fireEvent.click(await screen.findByRole('button', { name: 'Expand all diffs' }));
  expect(screen.getByRole('button', { name: 'Diff for skill:reviewed' }).getAttribute('aria-expanded')).toBe('true');
  fireEvent.click(screen.getByRole('button', { name: 'Restore selected' }));
  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getByText('Restore 1 selected items?')).toBeTruthy();
  fireEvent.click(button(dialog, 'Expand all diffs'));
  expect(button(dialog, 'skill:reviewed').getAttribute('aria-expanded')).toBe('true');
  fireEvent.click(button(dialog, 'Collapse all'));
  expect(button(dialog, 'skill:reviewed').getAttribute('aria-expanded')).toBe('false');
});
it('only offers Pull when behind or blocked and places blocked Pull in the notice', async () => {
  const view = mount();
  expect(screen.queryByRole('button', { name: 'Pull Baseline' })).toBeNull();
  view.rerender(<Draft revision={0} status={{ ...status, report: { ...status.report, behind: true } }} changed={changed} />);
  expect(screen.getByRole('button', { name: 'Pull Baseline' })).toBeTruthy();
  view.rerender(<Draft revision={0} status={{ ...status, report: { ...status.report, blocked: true } }} changed={changed} />);
  expect(screen.getAllByRole('button', { name: 'Pull Baseline' })).toHaveLength(1);
});
it('keeps one Push action after an unpushed commit and marks matching Draft items', async () => {
  vi.mocked(api.commit).mockResolvedValue({ sha: 'abc', items: ['skill:reviewed'], message: 'done', pushed: false, push_error: 'Offline' });
  render(<Draft revision={0} status={{ ...status, report: { ...status.report, unpushed: ['skill:reviewed', 'rule:committed'] } }} changed={changed} />);
  await screen.findByText('Unpushed');
  fireEvent.click(screen.getByRole('button', { name: 'Commit (1)' }));
  const dialog = await screen.findByRole('dialog');
  fireEvent.click(button(dialog, 'Commit'));
  await screen.findByText('abc');
  expect(screen.getAllByRole('button', { name: 'Push now' })).toHaveLength(1);
  expect(screen.queryByRole('button', { name: 'Retry push' })).toBeNull();
  expect(screen.getByText('2 unpushed')).toBeTruthy();
});
it('dismisses a successful commit notice', async () => {
  mount();
  fireEvent.click(await screen.findByRole('button', { name: 'Commit (1)' }));
  fireEvent.click(button(await screen.findByRole('dialog'), 'Commit'));
  fireEvent.click(await screen.findByRole('button', { name: 'Dismiss' }));
  expect(screen.queryByText('Commit complete')).toBeNull();
});
