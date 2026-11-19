// @vitest-environment jsdom
import { expect, it, vi } from 'vitest';
import { mockIPC } from '@tauri-apps/api/mocks';
import { installMock } from './mock';
import type * as T from '../types';

vi.mock('@tauri-apps/api/mocks', () => ({ mockIPC: vi.fn() }));

it('provides reviewed dev workflows and rejects unknown commands', async () => {
  vi.useFakeTimers();
  try {
    installMock('fleet');
    const call = vi.mocked(mockIPC).mock.calls[0][0];
    const run = async <Result,>(name: string, args: Record<string, unknown> = {}) => {
      const result = Promise.resolve(call(name, args)) as Promise<Result>;
      // Install the rejection handler before advancing the fake clock.
      if (name === 'unknown' || name === 'copy_machine_assignment') void result.catch(() => {});
      await vi.runAllTimersAsync();
      return result;
    };

    const copy = { source: 'laptop', mode: 'replace', harnesses: [{ from: 'claude', to: 'claude' }] };
    const preview = await run<T.AssignmentCopyPreview>('preview_assignment_copy', { machine: 'workstation', copy });
    expect(preview.count).toBeGreaterThan(0);
    await expect(run('copy_machine_assignment', { machine: 'workstation', copy: {
      ...copy, expected: { ...preview.target, registration: 'stale' },
    } })).rejects.toMatchObject({ status: 409 });
    await expect(run('copy_machine_assignment', { machine: 'workstation', copy: {
      ...copy, expected: preview.target, source_expected: preview.source,
    } })).resolves.toMatchObject({ revision: preview.target.revision + 1 });

    const request = { link: 'https://github.com/example/skills', ref: 'v2', path: 'skills/pdf' };
    const resolved = await run<T.ResolvedSource>('resolve_import', { request });
    expect(resolved.git_ref).toBe('v2');
    expect(resolved.candidates[0]).toMatchObject({ path: 'skills/pdf', total_bytes: 2048, warnings: expect.any(Array) });
    const imports = await run<T.ImportsView>('imports');
    const { name, upstream } = imports.updates[0];
    await expect(run('import_changes', { name, upstream })).resolves.toMatchObject({ upstream, files: ['SKILL.md'] });

    const query = { item: 'skill:code-review', path: 'SKILL.md' };
    const file = await run<T.FileContent>('skill_file', { query });
    expect(file.fingerprint).toBeTruthy();
    await run('write_skill_file', { request: { ...query, expected: file.fingerprint, content: 'Edited text' } });
    await expect(run('skill_file', { query })).resolves.toMatchObject({ text: 'Edited text', fingerprint: expect.any(String) });

    const historical = await run<T.FileContent>('skill_file', { query: { ...query, hash: 'sha256:two' } });
    expect(historical.text).toContain('Collect detailed notes.');
    expect(historical.fingerprint).toBeNull();

    const change = { op: 'disable', name: 'claude' };
    const harness = await run<T.HarnessPreview>('preview_harness', { request: { change } });
    expect(harness.fingerprint).toBeTruthy();
    await expect(run('apply_harness', { request: { change, fingerprint: harness.fingerprint } })).resolves.toMatchObject({ errors: [] });
    const sync = await run<T.SyncResult>('preview_sync');
    expect(sync.fingerprint).toBeTruthy();
    await expect(run('sync_now', { fingerprint: sync.fingerprint })).resolves.not.toHaveProperty('fingerprint');
    await expect(run('unknown')).rejects.toMatchObject({ message: 'Unhandled mock command: unknown' });
  } finally {
    vi.useRealTimers();
  }
});
