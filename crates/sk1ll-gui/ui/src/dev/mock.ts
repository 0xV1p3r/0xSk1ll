// Development-only fake Daemon for rendering the GUI in a plain browser (`?mock=<scenario>`).
import { mockIPC } from '@tauri-apps/api/mocks';
import type * as T from '../types';

const now = Math.floor(Date.now() / 1000);
const report = (overrides: Partial<T.MachineReport> = {}): T.MachineReport => ({
  baseline: '9f3c2a17d4e1', drafts: [], unpushed: [], conflicts: [], harnesses: ['claude', 'codex'],
  blocked: false, clashes: [], behind: false, ...overrides,
});
const harness = (skills: Record<string, boolean> = {}, rules: Record<string, boolean> = {}): T.HarnessAssignment =>
  ({ new_skills_default: true, new_rules_default: false, skills, rules });

export function installMock(scenario: string) {
  const empty = scenario === 'empty';
  const local = report({ drafts: empty ? [] : ['skill:release-notes', 'rule:style'], unpushed: empty ? [] : ['skill:release-notes'],
    conflicts: empty ? [] : [{ item: 'skill:code-review', harness: 'codex' }] });
  const status: T.StatusView = {
    machine_id: 'workstation', server_url: 'https://skills.example.com', repo: '/home/dev/.local/share/sk1ll/repo',
    headline: empty ? 'in_sync' : 'conflict', connection: scenario === 'auth' ? 'auth_rejected' : 'connected',
    last_error: scenario === 'auth' ? 'Server returned 401 Unauthorized' : null, revoked: scenario === 'revoked',
    server_baseline: '9f3c2a17d4e1', assignment_revision: 42, awaiting_assignment: scenario === 'awaiting',
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
    { item: 'skill:deploy', in_baseline: true, draft: null, unpushed: false, description: 'Builds and ships the server container.' },
    { item: 'skill:release-notes', in_baseline: false, draft: 'added', unpushed: true, description: 'Drafts release notes from merged pull requests.' },
    { item: 'rule:style', in_baseline: true, draft: 'modified', unpushed: false, description: 'House code style rules shared by every Harness.' },
    { item: 'rule:security', in_baseline: true, draft: null, unpushed: false, description: null },
  ];
  const content: T.ContentEntry[] = empty ? [] : [
    ...items.map((item): T.ContentEntry => {
      const [kind, name] = item.item.split(':');
      return { name, item: item.item, type: kind === 'rule' ? 'rule' : 'personal', description: item.description,
        draft: item.draft, in_baseline: item.in_baseline, path: `${status.repo}/${kind === 'rule' ? `instructions/${name}.md` : `personal/${name}`}`,
        enabled_harnesses: ['claude', 'codex'], clashes_with_repo: false, variants: [] };
    }),
    { name: 'pdf', item: 'skill:pdf', type: 'import', description: 'Work with PDFs.', draft: null, in_baseline: true,
      path: `${status.repo}/imported/pdf`, enabled_harnesses: ['claude'], clashes_with_repo: false, variants: [] },
    { name: 'notes', item: null, type: 'unmanaged', description: 'Collect notes.', draft: null, in_baseline: false,
      path: null, enabled_harnesses: [], clashes_with_repo: false, variants: [
        { hash: 'sha256:one', description: 'Collect notes.', locations: [{ harness: 'claude', path: '/home/dev/.claude/skills/notes' }] },
        { hash: 'sha256:two', description: 'Collect detailed notes.', locations: [{ harness: 'codex', path: '/home/dev/.codex/skills/notes' }] },
      ] },
  ];
  let preferredEditor = 'code';
  const assignments: Record<string, T.AssignmentSnapshot> = {
    workstation: { revision: 42, harnesses: { claude: harness({ deploy: false }, { style: true }), codex: harness({}, { security: true }) } },
    laptop: { revision: 17, harnesses: { claude: harness({ 'code-review': true }) } },
    'ci-runner': { revision: 9, harnesses: { codex: harness({}, {}) } },
    'build-box': { revision: 5, harnesses: { claude: harness(), codex: harness() } },
  };
  const draft: T.DraftEntry[] = empty ? [] : [
    { item: 'skill:release-notes', change: 'added', paths: ['skills/release-notes/SKILL.md'], fingerprint: 'a1' },
    { item: 'rule:style', change: 'modified', paths: ['rules/style.md'], fingerprint: 'b2' },
  ];
  const diffs: Record<string, string> = {
    'skill:release-notes': '@@ -0,0 +1,6 @@\n+---\n+name: release-notes\n+description: Drafts release notes from merged pull requests.\n+---\n+\n+Group changes by area and link each pull request.',
    'rule:style': '@@ -3,4 +3,5 @@\n Prefer small functions.\n-Use tabs for indentation.\n+Use two spaces for indentation.\n+Wrap lines at 120 columns.\n Name things for what they do.',
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
    harness: ['claude', 'codex'][i % 2], kind: i === 3 ? 'new_skills_default' : i % 2 ? 'rule' : 'skill',
    name: i === 3 ? null : ['deploy', 'style', 'code-review', null, 'security', 'release-notes', 'deploy', 'style'][i],
    before: i % 2 ? null : true, after: i % 2 ? true : false,
    cause: i === 5 ? { type: 'undo', entry: 101 } : i === 6 ? { type: 'harness_added' } : { type: 'change' },
  } as T.LogEntry));
  const harnesses: T.HarnessView[] = [
    { name: 'claude', origin: 'detected', harness: { skills: '~/.claude/skills', instructions: '~/.claude/CLAUDE.md', instructions_format: 'block', mode: 'symlink' } },
    { name: 'codex', origin: 'machine.toml', harness: { skills: '~/.codex/skills', instructions: '~/.codex/AGENTS.md', instructions_format: 'block', mode: 'copy' } },
  ];
  const imports: T.ImportsView & { imports: T.ImportEntry[] } = { imports: empty ? [] : [{ name: 'pdf', source: 'github:example/skills', ref: 'main', path: 'skills/pdf', locked: '4e5f6a7b8c9d' }, { name: 'anthropic-skills', source: 'https://github.com/anthropics/skills', ref: 'main', path: '', locked: '4e5f6a7b8c9d' }, { name: 'internal-skills', source: 'github:example/internal-skills', ref: 'main', path: '', locked: '123456789abc' }], checked_at: now - 900, errors: empty ? [] : [{ name: 'internal-skills', message: 'git fetch failed: repository not found' }],
    updates: empty ? [] : [{ name: 'anthropic-skills', source: 'https://github.com/anthropics/skills', ref: 'main', locked: '4e5f6a7b8c9d', upstream: 'c0ffee123456' }] };
  const delay = <V,>(value: V, ms = 250) => new Promise<V>(resolve => setTimeout(() => resolve(value), ms));

  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (scenario === 'unreachable' && cmd !== 'connection_info' && !cmd.startsWith('plugin:'))
      return Promise.reject({ message: 'connect ECONNREFUSED /run/user/1000/sk1ll/daemon.sock', status: null, unreachable: true });
    switch (cmd) {
      case 'push': {
        const pushed = local.unpushed.length > 0; local.unpushed = [];
        items.forEach(item => { item.unpushed = false; });
        return delay({ pushed });
      }
      case 'set_preferred_editor':
        if (!['code', 'nvim', 'file-manager'].includes(a.editorId as string)) return Promise.reject({ message: 'Editor is not available' });
        preferredEditor = a.editorId as string; return delay(undefined);
      case 'create_item': {
        const r = a.request as T.CreateItemRequest;
        if (!/^[a-zA-Z0-9_-][a-zA-Z0-9_.-]*$/.test(r.name) || !['skill', 'rule'].includes(r.kind)) return Promise.reject({ message: 'Invalid item name or kind' });
        const item = `${r.kind}:${r.name}`;
        if (content.some(e => e.item === item)) return Promise.reject({ message: 'Name is taken' });
        const path = r.kind === 'skill' ? `personal/${r.name}/SKILL.md` : `instructions/${r.name}.md`;
        content.push({ name: r.name, item, type: r.kind === 'skill' ? 'personal' : 'rule', description: r.description ?? null, draft: 'added', in_baseline: false, path: `${status.repo}/${path}`, enabled_harnesses: [], clashes_with_repo: false, variants: [] });
        items.push({ item, in_baseline: false, draft: 'added', unpushed: false, description: r.description ?? null });
        draft.push({ item, change: 'added', paths: [path], fingerprint: `created-${r.name}` }); local.drafts.push(item);
        return delay(undefined);
      }
      case 'conflict_diff': {
        const r = a.request as T.ConflictDiffRequest;
        const v = r.id ? recovered.find(v => v.id === r.id) : conflicts.find(v => v.harness === r.harness && v.name === r.name);
        if (!v) return Promise.reject({ message: 'Conflict no longer exists' });
        return delay({ item: v.items[0] ?? `skill:${v.name}`, diff: '--- Harness\n+++ Draft\n@@ -1 +1 @@\n-Harness content\n+Draft content\n' });
      }
      case 'remove_import': {
        const index = imports.imports.findIndex(i => i.name === a.name);
        if (index < 0) return Promise.reject({ message: 'Import no longer exists' });
        imports.imports.splice(index, 1); imports.updates = imports.updates.filter(i => i.name !== a.name);
        const item = `skill:${a.name}`;
        const ci = content.findIndex(e => e.item === item); if (ci >= 0) content.splice(ci, 1);
        draft.push({ item, change: 'deleted', paths: [`imported/${a.name}/SKILL.md`], fingerprint: 'removed' }); local.drafts.push(item);
        return delay(undefined);
      }
      case 'update_import': {
        const entry = imports.imports.find(i => i.name === a.name);
        if (!entry) return Promise.reject({ message: 'Import no longer exists' });
        const update = imports.updates.find(i => i.name === a.name);
        const old = entry.locked; entry.locked = update?.upstream ?? entry.locked;
        imports.updates = imports.updates.filter(i => i.name !== a.name);
        return delay({ name: entry.name, old_commit: old, new_commit: entry.locked ?? '', changed: old !== entry.locked });
      }
      case 'check_imports': imports.checked_at = Math.floor(Date.now() / 1000); return delay(imports);
      case 'status': return delay(status, 60);
      case 'fleet': return delay({ baseline: '9f3c2a17d4e1', machines: empty ? machines.slice(0, 1) : machines });
      case 'items': return delay(items);
      case 'skills': return delay(structuredClone(content));
      case 'skill_tree': {
        const query = a.query as T.ContentQuery;
        return delay(query.item.startsWith('rule:') ? [{ path: `${query.item.slice(9)}.md`, kind: 'file', size: 120 }]
          : [{ path: 'SKILL.md', kind: 'file', size: 120 }, { path: 'references', kind: 'directory', size: 0 }, { path: 'references/example.md', kind: 'file', size: 50 }]);
      }
      case 'skill_file': {
        const query = a.query as T.ContentQuery & { path: string };
        return delay({ path: query.path, text: `---\nname: ${query.item.split(':')[1]}\ndescription: ${query.hash === 'sha256:two' ? 'Collect detailed notes.' : 'Work with this Skill.'}\n---\n# Instructions\n\nUse **careful review** before making changes.\n\n- Read the files.\n- Check the result.`, binary: false, too_large: false, size: 120 });
      }
      case 'editors': return delay({ preferred_editor: preferredEditor, editors: [
        { id: 'code', name: 'VS Code', kind: 'gui' }, { id: 'nvim', name: 'Neovim', kind: 'terminal' }, { id: 'file-manager', name: 'File manager', kind: 'system' },
      ] });
      case 'open_in_editor': preferredEditor = a.editorId as string; return delay(undefined);
      case 'fork': case 'adopt_unmanaged': {
        const name = cmd === 'fork' ? a.name as string : (a.target as T.ConflictTarget).name;
        const entry = content.find(value => value.name === name && value.type === (cmd === 'fork' ? 'import' : 'unmanaged'));
        if (!entry) return Promise.reject({ message: 'Skill no longer exists' });
        entry.type = 'personal'; entry.item = `skill:${name}`; entry.draft = cmd === 'fork' ? 'modified' : 'added';
        entry.path = `${status.repo}/personal/${name}`; entry.variants = []; entry.enabled_harnesses = ['claude'];
        if (!items.some(item => item.item === entry.item)) items.push({ item: entry.item, in_baseline: false, draft: entry.draft, unpushed: false, description: entry.description });
        draft.push({ item: entry.item, change: entry.draft, paths: [`personal/${name}/SKILL.md`], fingerprint: 'converted' });
        local.drafts.push(entry.item);
        return delay(undefined);
      }
      case 'assignment': return delay(assignments[a.machine as string] ?? { revision: 0, harnesses: {} });
      case 'set_assignment': {
        const snapshot = assignments[a.machine as string];
        for (const change of (a.changes as T.AssignmentChanges).changes) {
          const h = snapshot.harnesses[change.harness] ??= harness();
          if (change.kind === 'skill' || change.kind === 'rule') {
            const map = change.kind === 'skill' ? h.skills : h.rules;
            if (change.enabled === null) delete map[change.name!]; else map[change.name!] = change.enabled;
          } else h[change.kind] = !!change.enabled;
        }
        snapshot.revision++;
        const { revision, ...assignment } = snapshot;
        return delay({ revision, assignment: structuredClone(assignment), entries: [] }, 400);
      }
      case 'draft': return delay(draft);
      case 'diff': return delay({ item: a.item, diff: diffs[a.item as string] ?? '' }, 50);
      case 'commit_message': return delay({ items: a.items, message: `Commit ${(a.items as string[]).join(', ')}` });
      case 'commit': return delay({ sha: '7d1e9a0b3c5f8e2d', items: [], message: '', pushed: true, push_error: null }, 600);
      case 'restore': return delay({ discarded: [] });
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
        imports.imports.push({ name: request.name, source: request.source, ref: request.ref, path: request.path, locked: request.commit ?? 'c0ffee1234567890' });
        content.push({ name: request.name, item: `skill:${request.name}`, type: 'import', description: null, draft: 'added', in_baseline: false, path: `${status.repo}/imported/${request.name}`, enabled_harnesses: [], clashes_with_repo: false, variants: [] });
        items.push({ item: `skill:${request.name}`, in_baseline: false, draft: 'added', unpushed: false, description: null });
        draft.push({ item: `skill:${request.name}`, change: 'added', paths: [`imported/${request.name}/SKILL.md`], fingerprint: 'new' });
        return delay({ name: request.name, old_commit: null, new_commit: 'c0ffee1234567890', changed: true });
      }
      case 'log': return delay(log);
      case 'join_code': return delay({ code: 'sk1ll-join-7Q2M-XK9P-44RD-ZT8W' });
      case 'connection_info': return delay('/run/user/1000/sk1ll/daemon.sock', 10);
      case 'adopt': case 'overwrite': case 'adopt_displaced': case 'discard_displaced': case 'undo':
      case 'remove_machine': case 'join': return delay(undefined, 500);
      default: return undefined;
    }
  }, { shouldMockEvents: true });
}
