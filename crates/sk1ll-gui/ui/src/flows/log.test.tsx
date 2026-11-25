// The Assignment log: reading it page by page, filtering it on the Server, and restoring an entry's previous value.
import { expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import { dialog, heading, isDisabled, launch, notify } from './harness';
import type { LogCause, LogEntry } from '../types';
import type { MockDaemon } from '../dev/mock';

const causes: LogCause[] = [{ type: 'undo', entry: 101 }, { type: 'orphan_pruned' }, { type: 'default_materialized' }, { type: 'machine_removed' }, { type: 'harness_added' }];
// Entries #100 down to #41, newest first; the first five carry each kind of cause.
const sixtyEntries = (daemon: MockDaemon) => daemon.log.splice(0, daemon.log.length, ...Array.from({ length: 60 }, (_, i): LogEntry => ({
  id: 100 - i, at: 1_700_000_000 - i * 60, author: i % 2 ? 'ops@laptop' : 'dev@workstation', machine: i % 2 ? 'laptop' : 'workstation',
  harness: 'claude', kind: 'skill', item_id: i === 7 ? 'id-pdf' : 'id-deploy', name: i === 7 ? 'pdf' : 'deploy', before: true, after: false,
  cause: causes[i] ?? { type: 'change' },
})));
const shown = (id: number) => screen.queryByText(`#${id}`);

it('pages through the log, filters it on the Server, and starts again from the newest on every new filter', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-log', setup: sixtyEntries });
  await heading('Assignment log');
  expect(await screen.findByText('#100')).toBeTruthy();
  for (const label of ['Restored #101', 'Orphan cleared', 'Default applied', 'Machine removed', 'Harness added']) expect(screen.getAllByText(label).length).toBeGreaterThan(0);
  expect(screen.getByRole('option', { name: 'laptop' })).toBeTruthy();
  expect(screen.getByRole('option', { name: 'ops@laptop' })).toBeTruthy();

  await user.click(screen.getByRole('button', { name: /Older/ }));
  expect(await screen.findByText('#50')).toBeTruthy();
  expect(shown(100)).toBeNull();
  expect(screen.getByText('Page 2')).toBeTruthy();
  expect(daemon.sent('log').at(-1)).toEqual({ query: { before: 51, limit: 50, machine: '', harness: '', author: '', item: '' } });

  // A typed search holds paging until it applies, so a cursor from the old results is never reused.
  await user.click(screen.getByLabelText('Search items'));
  await user.paste('missing');
  expect(isDisabled(screen.getByRole('button', { name: /Older/ }))).toBe(true);
  expect(await screen.findByText('No entries match these filters')).toBeTruthy();
  expect(daemon.sent('log').at(-1)).toEqual({ query: { before: undefined, limit: 50, machine: '', harness: '', author: '', item: 'missing' } });
  expect(screen.queryByText('Page 2')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Clear filters' }));
  expect(await screen.findByText('#100')).toBeTruthy();
  expect(screen.getByText('Page 1')).toBeTruthy();

  await user.selectOptions(screen.getByLabelText('Filter by machine'), 'laptop');
  await waitFor(() => expect(shown(100)).toBeNull());
  expect(shown(99)).toBeTruthy();
  await user.click(screen.getByLabelText('Search items'));
  await user.paste('pdf');
  await waitFor(() => expect(shown(99)).toBeNull());
  expect(shown(93)).toBeTruthy();
});

it('refreshes its filters from the Fleet, and restores an entry\'s previous value as a new entry', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-log' });
  await heading('Assignment log');
  await screen.findByText('#120');
  expect(screen.queryByRole('option', { name: 'remote' })).toBeNull();
  daemon.log.unshift({ ...daemon.log[0], id: 121, machine: 'remote' });
  await notify('fleet');
  expect(await screen.findByRole('option', { name: 'remote' })).toBeTruthy();
  // `remote` isn't in the Fleet any more: its entries can't be restored.
  expect(screen.getAllByText('Machine removed').length).toBeGreaterThan(0);

  await user.click(screen.getByRole('button', { name: /^Restore On for log entry 120: skill:deploy on workstation \/ claude/ }));
  await user.click((await dialog('Restore Assignment?')).getByRole('button', { name: 'Restore' }));
  expect(await screen.findByText('Restored Assignment skill:deploy on workstation / claude to On.')).toBeTruthy();
  expect(daemon.sent('undo')).toEqual([{ entry: 120 }]);
  expect(await screen.findByText('#122')).toBeTruthy();
  expect(screen.getAllByText('Restored #120').length).toBeGreaterThan(0);
});
