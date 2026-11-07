// Development-only fake Daemon for rendering the GUI in a plain browser (`?mock=<scenario>`).
import { mockIPC } from '@tauri-apps/api/mocks';
import type * as T from '../types';

const now = Math.floor(Date.now() / 1000);
const report = (overrides: Partial<T.MachineReport> = {}): T.MachineReport => ({
  baseline: '9f3c2a17d4e1', drafts: [], unpushed: [], conflicts: [], harnesses: ['claude', 'codex'],
  blocked: false, clashes: [], behind: false, ...overrides,
});
const harness = (skills: Record<string, boolean> = {}, fragments: Record<string, boolean> = {}): T.HarnessAssignment =>
  ({ new_skills_default: true, new_fragments_default: false, skills, fragments });

export function installMock(scenario: string) {
  const empty = scenario === 'empty';
  const local = report({ drafts: empty ? [] : ['skill:release-notes', 'fragment:style'], unpushed: empty ? [] : ['skill:release-notes'],
    conflicts: empty ? [] : [{ item: 'skill:code-review', harness: 'codex' }] });
  const status: T.StatusView = {
    machine_id: 'workstation', hub_url: 'https://skills.example.com', repo: '/home/dev/.local/share/sk1ll/repo',
    headline: empty ? 'in_sync' : 'conflict', connection: scenario === 'auth' ? 'auth_rejected' : 'connected',
    last_error: scenario === 'auth' ? 'Hub returned 401 Unauthorized' : null, revoked: scenario === 'revoked',
    hub_baseline: '9f3c2a17d4e1', assignment_revision: 42, awaiting_assignment: scenario === 'awaiting',
    auth_rejected_since: scenario === 'auth' ? now - 600 : null, pending_discards: 0, report: local,
  };
  const machines: T.MachineRow[] = [
    { id: 'workstation', status: status.headline, connected: true, joined_at: now - 86400 * 30, last_seen: now - 4, report: local },
    { id: 'laptop', status: 'behind', connected: true, joined_at: now - 86400 * 12, last_seen: now - 40,
      report: report({ baseline: '1b2c3d4e5f60', behind: true, harnesses: ['claude'] }) },
    { id: 'ci-runner', status: 'offline', connected: false, joined_at: now - 86400 * 3, last_seen: now - 7200,
      report: report({ harnesses: ['codex'] }) },
    { id: 'build-box', status: 'blocked', connected: true, joined_at: now - 86400 * 8, last_seen: now - 12,
      report: report({ blocked: true, clashes: ['skill:deploy'], drafts: ['skill:deploy'] }) },
  ];
  const items: T.ItemView[] = empty ? [] : [
    { item: 'skill:code-review', in_baseline: true, draft: null, unpushed: false, description: 'Reviews a diff for correctness bugs and suggests fixes.' },
    { item: 'skill:deploy', in_baseline: true, draft: null, unpushed: false, description: 'Builds and ships the hub container.' },
    { item: 'skill:release-notes', in_baseline: false, draft: 'added', unpushed: true, description: 'Drafts release notes from merged pull requests.' },
    { item: 'fragment:style', in_baseline: true, draft: 'modified', unpushed: false, description: 'House code style rules shared by every Harness.' },
    { item: 'fragment:security', in_baseline: true, draft: null, unpushed: false, description: null },
  ];
  const assignments: Record<string, T.AssignmentSnapshot> = {
    workstation: { revision: 42, harnesses: { claude: harness({ deploy: false }, { style: true }), codex: harness({}, { security: true }) } },
    laptop: { revision: 17, harnesses: { claude: harness({ 'code-review': true }) } },
    'ci-runner': { revision: 9, harnesses: { codex: harness({}, {}) } },
    'build-box': { revision: 5, harnesses: { claude: harness(), codex: harness() } },
  };
  const draft: T.DraftEntry[] = empty ? [] : [
    { item: 'skill:release-notes', change: 'added', paths: ['skills/release-notes/SKILL.md'], fingerprint: 'a1' },
    { item: 'fragment:style', change: 'modified', paths: ['fragments/style.md'], fingerprint: 'b2' },
  ];
  const diffs: Record<string, string> = {
    'skill:release-notes': '@@ -0,0 +1,6 @@\n+---\n+name: release-notes\n+description: Drafts release notes from merged pull requests.\n+---\n+\n+Group changes by area and link each pull request.',
    'fragment:style': '@@ -3,4 +3,5 @@\n Prefer small functions.\n-Use tabs for indentation.\n+Use two spaces for indentation.\n+Wrap lines at 120 columns.\n Name things for what they do.',
  };
  const conflicts: T.ConflictView[] = empty ? [] : [
    { harness: 'codex', name: 'code-review', items: ['skill:code-review'], dest: '/home/dev/.codex/skills/code-review',
      reason: 'SKILL.md was edited in the Harness after the last sync.', adoptable: true },
  ];
  const recovered: T.ConflictView[] = empty ? [] : [
    { kind: 'displaced', id: 'd-1', harness: 'claude', name: 'notes', items: [], dest: '/home/dev/.claude/skills/notes',
      path: '/home/dev/.local/share/sk1ll/recovered/d-1', displaced_at: now - 3600 * 5, reason: 'Unmanaged directory replaced by an assigned Skill.', adoptable: true },
  ];
  const log: T.LogEntry[] = empty ? [] : Array.from({ length: 8 }, (_, i) => ({
    id: 120 - i, at: now - i * 2400, author: i % 3 ? 'dev@workstation' : 'ops@laptop', machine: ['workstation', 'laptop'][i % 2],
    harness: ['claude', 'codex'][i % 2], kind: i === 3 ? 'new_skills_default' : i % 2 ? 'fragment' : 'skill',
    name: i === 3 ? null : ['deploy', 'style', 'code-review', null, 'security', 'release-notes', 'deploy', 'style'][i],
    before: i % 2 ? null : true, after: i % 2 ? true : false,
    cause: i === 5 ? { type: 'undo', entry: 101 } : i === 6 ? { type: 'harness_added' } : { type: 'change' },
  } as T.LogEntry));
  const harnesses: T.HarnessView[] = [
    { name: 'claude', origin: 'detected', harness: { skills: '~/.claude/skills', instructions: '~/.claude/CLAUDE.md', instructions_format: 'block', mode: 'symlink' } },
    { name: 'codex', origin: 'machine.toml', harness: { skills: '~/.codex/skills', instructions: '~/.codex/AGENTS.md', instructions_format: 'block', mode: 'copy' } },
  ];
  const imports: T.ImportsView = { checked_at: now - 900, errors: empty ? [] : [{ name: 'internal-skills', message: 'git fetch failed: repository not found' }],
    updates: empty ? [] : [{ name: 'anthropic-skills', source: 'https://github.com/anthropics/skills', ref: 'main', locked: '4e5f6a7b8c9d', upstream: 'c0ffee123456' }] };
  const delay = <V,>(value: V, ms = 250) => new Promise<V>(resolve => setTimeout(() => resolve(value), ms));

  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (scenario === 'unreachable' && cmd !== 'connection_info' && !cmd.startsWith('plugin:'))
      return Promise.reject({ message: 'connect ECONNREFUSED /run/user/1000/sk1ll/daemon.sock', status: null, unreachable: true });
    switch (cmd) {
      case 'status': return delay(status, 60);
      case 'fleet': return delay({ baseline: '9f3c2a17d4e1', machines: empty ? machines.slice(0, 1) : machines });
      case 'items': return delay(items);
      case 'assignment': return delay(assignments[a.machine as string] ?? { revision: 0, harnesses: {} });
      case 'set_assignment': {
        const snapshot = assignments[a.machine as string];
        for (const change of (a.changes as T.AssignmentChanges).changes) {
          const h = snapshot.harnesses[change.harness] ??= harness();
          if (change.kind === 'skill' || change.kind === 'fragment') {
            const map = change.kind === 'skill' ? h.skills : h.fragments;
            if (change.enabled === null) delete map[change.name!]; else map[change.name!] = change.enabled;
          } else h[change.kind] = !!change.enabled;
        }
        snapshot.revision++;
        const { revision, ...assignment } = snapshot;
        return delay({ revision, assignment: structuredClone(assignment), entries: [] }, 400);
      }
      case 'draft': return delay(draft);
      case 'diff': return delay({ item: a.item, diff: diffs[a.item as string] ?? '' }, 50);
      case 'promote_message': return delay({ items: a.items, message: `Promote ${(a.items as string[]).join(', ')}` });
      case 'promote': return delay({ sha: '7d1e9a0b3c5f8e2d', items: [], message: '', pushed: true, push_error: null }, 600);
      case 'reset': return delay({ discarded: [] });
      case 'pull': return delay({ state: 'up_to_date' }, 500);
      case 'conflicts': return delay(conflicts);
      case 'recovered': return delay(recovered);
      case 'harnesses': return delay(harnesses);
      case 'imports': return delay(imports);
      // A tree/blob link resolves one Skill; a repository link resolves several.
      case 'resolve_import': {
        const request = a.request as T.ResolveRequest;
        const single = /\/(tree|blob)\//.test(request.link);
        return delay({ source: 'github:example/skills', git_ref: 'main', commit: 'c0ffee1234567890',
          candidates: (single ? ['pdf'] : ['pdf', 'writing']).map(name => ({ path: `skills/${name}`,
            default_name: name, description: `Tools for ${name}.`, taken: items.some(i => i.item === `skill:${name}`) })) });
      }
      case 'add_import': {
        const request = a.request as T.AddImportRequest;
        if (items.some(i => i.item === `skill:${request.name}`))
          return Promise.reject({ message: 'Import name already exists', status: 409, unreachable: false });
        items.push({ item: `skill:${request.name}`, in_baseline: false, draft: 'added', unpushed: false, description: null });
        draft.push({ item: `skill:${request.name}`, change: 'added', paths: [`imported/${request.name}/SKILL.md`], fingerprint: 'new' });
        return delay({ name: request.name, old_commit: null, new_commit: 'c0ffee1234567890', changed: true });
      }
      case 'log': return delay(log);
      case 'join_code': return delay({ code: 'sk1ll-join-7Q2M-XK9P-44RD-ZT8W' });
      case 'connection_info': return delay('/run/user/1000/sk1ll/daemon.sock', 10);
      case 'adopt': case 'overwrite': case 'adopt_displaced': case 'discard_displaced': case 'undo':
      case 'update_import': case 'remove_machine': case 'join': return delay(undefined, 500);
      default: return undefined;
    }
  }, { shouldMockEvents: true });
}
