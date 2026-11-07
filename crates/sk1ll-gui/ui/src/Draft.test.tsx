// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Draft } from './Draft';
import { Confirm } from './shared';
import { api } from './api';
import type { DraftEntry, StatusView } from './types';

vi.mock('./api', () => ({ api: {
  draft: vi.fn(), diff: vi.fn(), promoteMessage: vi.fn(), promote: vi.fn(), reset: vi.fn(), pull: vi.fn(),
}, errorMessage: (error: unknown) => String(error) }));
const status = { report: { blocked: false, clashes: [] } } as unknown as StatusView;
const entry = (item: string): DraftEntry => ({ item, change: 'added', paths: [`${item}/SKILL.md`], fingerprint: 'initial' });
let entries: DraftEntry[];
let diffs: Record<string, string>;
const changed = vi.fn();
beforeEach(() => {
  vi.resetAllMocks();
  entries = [entry('skill:reviewed')]; diffs = { 'skill:reviewed': '+first version' };
  vi.mocked(api.draft).mockImplementation(async () => structuredClone(entries));
  vi.mocked(api.diff).mockImplementation(async item => ({ item, diff: diffs[item] ?? '+new' }));
  vi.mocked(api.promoteMessage).mockResolvedValue({ items: ['skill:reviewed'], message: 'Promote reviewed' });
  vi.mocked(api.reset).mockResolvedValue({ discarded: ['skill:reviewed'] });
  vi.mocked(api.promote).mockResolvedValue({ sha: 'abc', items: ['skill:reviewed'], message: 'done', pushed: true, push_error: null });
});
afterEach(cleanup);
const mount = () => render(<Draft revision={0} status={status} changed={changed} />);
const refresh = async (view: ReturnType<typeof mount>, revision: number) => {
  view.rerender(<Draft revision={revision} status={status} changed={changed} />);
  await waitFor(() => expect(api.draft).toHaveBeenCalledTimes(revision + 1));
};
const button = (root: HTMLElement, name: string) => within(root).getByRole('button', { name }) as HTMLButtonElement;

describe('Draft confirmations', () => {
  it('freezes Reset all items and requires review when a new item arrives', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('button', { name: 'Reset all' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/Return skill:reviewed/)).toBeTruthy();
    entries.push(entry('skill:new'));
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Reset all').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:new.*added/)).toBeTruthy();
    expect(api.reset).not.toHaveBeenCalled();
    fireEvent.click(button(dialog, 'Review updated Draft'));
    await waitFor(() => expect(button(dialog, 'Reset all').disabled).toBe(false));
    fireEvent.click(button(dialog, 'Reset all'));
    await waitFor(() => expect(api.reset).toHaveBeenCalledWith({ items: ['skill:reviewed'], all: false }));
  });
  it('invalidates Reset selected when content changes at the same paths', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select skill:reviewed' }));
    fireEvent.click(screen.getByRole('button', { name: 'Reset selected' }));
    const dialog = await screen.findByRole('dialog');
    diffs['skill:reviewed'] = '+second version';
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Reset selected').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
  });
  it('revalidates Promote content and missing items and keeps the edited message on review', async () => {
    entries.push(entry('fragment:gone'));
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Check all items' }));
    fireEvent.click(screen.getByRole('button', { name: 'Promote (2)' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByRole('textbox'), { target: { value: 'My message' } });
    diffs['skill:reviewed'] = '+edited again'; entries = [entry('skill:reviewed')];
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Promote').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
    expect(within(dialog).getByText(/fragment:gone.*no longer/)).toBeTruthy();
    fireEvent.click(button(dialog, 'Review updated Draft'));
    fireEvent.click(button(dialog, 'Promote'));
    await waitFor(() => expect(api.promote).toHaveBeenCalledWith({ items: ['skill:reviewed'], message: 'My message' }));
  });
  it('invalidates review for binary edits whose textual diff is unchanged', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select skill:reviewed' }));
    fireEvent.click(screen.getByRole('button', { name: 'Promote (1)' }));
    const dialog = await screen.findByRole('dialog');
    entries[0].fingerprint = 'binary-content-changed';
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Promote').disabled).toBe(true));
    expect(within(dialog).getByText(/skill:reviewed.*changed/)).toBeTruthy();
    expect(api.promote).not.toHaveBeenCalled();
  });
  it('cannot reconfirm Promote when all selected items disappeared', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select skill:reviewed' }));
    fireEvent.click(screen.getByRole('button', { name: 'Promote (1)' }));
    const dialog = await screen.findByRole('dialog');
    entries = [];
    await refresh(view, 1);
    await waitFor(() => expect(button(dialog, 'Promote').disabled).toBe(true));
    fireEvent.click(button(dialog, 'Review updated Draft'));
    expect(button(dialog, 'Promote').disabled).toBe(true);
    expect(api.promote).not.toHaveBeenCalled();
  });
  it('keeps review invalidated even if a later refresh restores the original content', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select skill:reviewed' }));
    fireEvent.click(screen.getByRole('button', { name: 'Reset selected' }));
    const dialog = await screen.findByRole('dialog');
    diffs['skill:reviewed'] = '+temporary edit';
    await refresh(view, 1);
    await within(dialog).findByText(/skill:reviewed.*changed/);
    diffs['skill:reviewed'] = '+first version';
    await refresh(view, 2);
    expect(button(dialog, 'Reset selected').disabled).toBe(true);
  });
  it('blocks confirmation and shows errors when a Draft refresh fails', async () => {
    const view = mount();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select skill:reviewed' }));
    fireEvent.click(screen.getByRole('button', { name: 'Promote (1)' }));
    const dialog = await screen.findByRole('dialog');
    vi.mocked(api.draft).mockRejectedValueOnce(new Error('Draft refresh failed'));
    await refresh(view, 1);
    await within(dialog).findByText(/Draft refresh failed/);
    expect(button(dialog, 'Promote').disabled).toBe(true);
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
