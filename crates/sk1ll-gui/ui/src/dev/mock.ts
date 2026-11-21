// Development-only fake Daemon for rendering the GUI in a plain browser (`?mock=<scenario>`).
import { mockIPC } from '@tauri-apps/api/mocks';
import type * as T from '../types';

const now = Math.floor(Date.now() / 1000);
const report = (overrides: Partial<T.MachineReport> = {}): T.MachineReport => ({
  seq: 1, items: [], inventory_complete: true, baseline: '9f3c2a17d4e1', drafts: [], unpushed: [], conflicts: [], harnesses: ['claude', 'codex'],
  blocked: false, clashes: [], behind: false, ...overrides,
});
const harness = (skills: Record<string, boolean> = {}, rules: Record<string, boolean> = {}): T.HarnessAssignment =>
  ({ new_skills_default: true, new_rules_default: false, skills, rules });

export function installMock(scenario: string) {
  let standalone = scenario.startsWith('standalone');
  const empty = scenario === 'empty' || scenario === 'standalone-first-run';
  const local = report({ drafts: empty ? [] : ['skill:release-notes', 'rule:style'], unpushed: empty ? [] : ['skill:release-notes'],
    conflicts: empty ? [] : [{ item: 'skill:code-review', harness: 'codex' }] });
  const status: T.StatusView = {
    mode: standalone ? 'standalone' : 'joined', has_origin: !standalone || scenario === 'standalone-origin', repo_ready: scenario !== 'standalone-first-run',
    machine_id: 'workstation', server_url: standalone ? null : 'https://skills.example.com', repo: '/home/dev/.local/share/sk1ll/repo',
    headline: empty ? 'in_sync' : 'conflict', connection: standalone ? 'standalone' : scenario === 'auth' ? 'auth_rejected' : 'connected',
    last_error: scenario === 'auth' ? 'Server returned 401 Unauthorized' : null, revoked: scenario === 'revoked',
    server_baseline: '9f3c2a17d4e1', assignment_revision: 42, awaiting_assignment: scenario === 'awaiting',
    auth_rejected_since: scenario === 'auth' ? now - 600 : null, report: local,
  };
  const machines: T.MachineRow[] = [
    { id: 'workstation', scope: scenario === 'self' ? 'self' : 'fleet', registration: 'reg-workstation', status: status.headline, connected: true, joined_at: now - 86400 * 30, last_seen: now - 4, report: local },
    { id: 'laptop', registration: 'reg-laptop', status: 'behind', connected: true, joined_at: now - 86400 * 12, last_seen: now - 40,
      report: report({ baseline: '1b2c3d4e5f60', behind: true, harnesses: ['claude'] }) },
    { id: 'ci-runner', registration: 'reg-ci-runner', status: 'offline', connected: false, joined_at: now - 86400 * 3, last_seen: now - 7200,
      report: report({ harnesses: ['codex'] }) },
    { id: 'build-box', registration: 'reg-build-box', status: 'blocked', connected: true, joined_at: now - 86400 * 8, last_seen: now - 12,
      report: report({ blocked: true, clashes: ['skill:deploy'], drafts: ['skill:deploy'] }) },
  ];
  const items: T.ItemView[] = empty ? [] : [
    { item: 'skill:code-review', id: 'id-code-review', in_baseline: true, draft: null, unpushed: false, description: 'Reviews a diff for correctness bugs and suggests fixes.' },
    { item: 'skill:deploy', id: 'id-deploy', in_baseline: true, draft: null, unpushed: false, description: 'Builds and ships the server container.' },
    { item: 'skill:release-notes', id: 'id-release-notes', in_baseline: false, draft: 'added', unpushed: true, description: 'Drafts release notes from merged pull requests.' },
    { item: 'rule:style', id: 'id-style', in_baseline: true, draft: 'modified', unpushed: false, description: 'House code style rules shared by every Harness.' },
    { item: 'rule:security', id: 'id-security', in_baseline: true, draft: null, unpushed: false, description: null },
  ];
  local.items = items.flatMap(({ item, id }) => id == null ? [] : [{ item, id }]);
  let nextId = 0;
  const newId = () => `new-${++nextId}`;
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
    workstation: { revision: 42, harnesses: { claude: harness({ 'id-deploy': false, 'id-old-skill': true }, { 'id-style': true }), codex: harness({}, { 'id-security': true }) } },
    laptop: { revision: 17, harnesses: { claude: harness({ 'id-code-review': true, ...scenario === 'self' && { 'id-laptop-orphan': true } }) } },
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
      reason: 'SKILL.md was edited in the Harness after the last sync.', adoptable: true, detected_at: now - 60 * 25 },
  ];
  const recovered: T.ConflictView[] = empty ? [] : [
    { kind: 'displaced', id: 'd-1', harness: 'claude', name: 'notes', items: [], dest: '/home/dev/.claude/skills/notes',
      path: '/home/dev/.local/share/sk1ll/recovered/d-1', displaced_at: now - 3600 * 5, reason: 'Unmanaged directory replaced by an assigned Skill.', adoptable: true },
  ];
  const log: T.LogEntry[] = empty ? [] : Array.from({ length: 8 }, (_, i) => ({
    id: 120 - i, at: now - i * 2400, author: i % 3 ? 'dev@workstation' : 'ops@laptop', machine: ['workstation', 'laptop'][i % 2],
    harness: ['claude', 'codex'][i % 2], kind: i === 3 ? 'new_skills_default' : i % 2 ? 'rule' : 'skill',
    item_id: i === 3 ? null : `id-${['deploy', 'style', 'code-review', null, 'security', 'release-notes', 'deploy', 'style'][i]}`,
    name: i === 3 ? null : ['deploy', 'style', 'code-review', null, 'security', 'release-notes', 'deploy', 'style'][i],
    before: i % 2 ? null : true, after: i % 2 ? true : false,
    cause: i === 5 ? { type: 'undo', entry: 101 } : i === 6 ? { type: 'harness_added' } : { type: 'change' },
  } as T.LogEntry));
  const orphanToggles = (): T.Orphan[] => {
    const known = new Set(items.filter(item => item.in_baseline || item.draft !== 'deleted').map(item => item.id));
    for (const machine of machines) {
      if (machine.id === status.machine_id) continue;
      for (const item of machine.report?.items ?? []) known.add(item.id);
    }
    return machines.flatMap(({ id: machine }) => Object.entries(assignments[machine]?.harnesses ?? {}).flatMap(([harness, h]) =>
      (['skill', 'rule'] as const).flatMap(kind => Object.keys(kind === 'skill' ? h.skills : h.rules)
        .filter(item_id => !known.has(item_id))
        .map(item_id => ({ machine, harness, kind, item_id, name: log.find(entry => entry.item_id === item_id && entry.name)?.name ?? null })))));
  };
  const harnesses: T.HarnessView[] = [
    { name: 'claude', origin: 'detected', harness: { skills: '~/.claude/skills', instructions: '~/.claude/CLAUDE.md', instructions_format: 'block', mode: 'symlink' } },
    { name: 'codex', origin: 'machine.toml', harness: { skills: '~/.codex/skills', instructions: '~/.codex/AGENTS.md', instructions_format: 'block', mode: 'copy' } },
  ];
  const imports: T.ImportsView & { imports: T.ImportEntry[] } = { imports: empty ? [] : [{ name: 'pdf', source: 'github:example/skills', ref: 'main', path: 'skills/pdf', locked: '4e5f6a7b8c9d' }, { name: 'anthropic-skills', source: 'https://github.com/anthropics/skills', ref: 'main', path: '', locked: '4e5f6a7b8c9d' }, { name: 'internal-skills', source: 'github:example/internal-skills', ref: 'main', path: '', locked: '123456789abc' }], checked_at: now - 900, errors: empty ? [] : [{ name: 'internal-skills', message: 'git fetch failed: repository not found' }],
    updates: empty ? [] : [{ name: 'anthropic-skills', source: 'https://github.com/anthropics/skills', ref: 'main', locked: '4e5f6a7b8c9d', upstream: 'c0ffee123456' }] };
  const setupHarnesses: T.HarnessChoice[] = [
    { name: 'claude', installed: true, enabled: true, preset: true, configured: false, harness: harnesses[0].harness },
    { name: 'codex', installed: true, enabled: true, preset: true, configured: true, harness: harnesses[1].harness },
    { name: 'gemini', installed: false, enabled: false, preset: true, configured: false, harness: { skills: '~/.gemini/skills', instructions: '~/.gemini/GEMINI.md', instructions_format: 'block', mode: 'symlink' } },
    { name: 'kiro', installed: true, enabled: false, preset: true, configured: false, available: true, harness: { skills: '~/.kiro/skills', instructions: '~/.kiro/steering', instructions_format: 'rules', mode: 'copy' } },
  ];
  const machineConfig = '/home/dev/.config/sk1ll/machine.toml';
  let notificationsEnabled = true;
  // Every enabled Harness got `code-review`; a change previews it as left in place or placed.
  const harnessPreview = (change: T.HarnessChange): T.HarnessPreview => {
    const choice = setupHarnesses.find(h => h.name === change.name);
    const before = choice?.enabled ? choice.harness : null;
    const after = change.op === 'set' ? change.harness
      : change.op === 'disable' || (change.op === 'remove' && !choice?.preset) ? null : choice?.harness ?? null;
    const placement = (h: T.HarnessView['harness'], kind: T.PlacementView['kind']): T.PlacementView[] => h.skills ? [{ harness: change.name, kind, item: 'skill', name: 'code-review', dest: `${h.skills}/code-review`, reason: null }] : [];
    const moved = before && (!after || after.skills !== before.skills);
    return { fingerprint: JSON.stringify(setupHarnesses), path: machineConfig, name: change.name, before, after, placed: moved ? placement(before, 'remove') : [],
      warnings: [], sync: after && (!before || after.skills !== before.skills) ? placement(after, 'create') : [], awaiting_assignment: false };
  };
  const editedFiles = new Map<string, { text: string; fingerprint: string }>();
  let fileRevision = 0;
  let running = scenario !== 'unreachable';
  const delay = <V,>(value: V, ms = 250) => new Promise<V>(resolve => setTimeout(() => resolve(value), ms));
  let syncPending = !empty;
  mockIPC((cmd, args) => {
    if (cmd === 'render_instructions') {
      if (empty) return [];
      const { name } = args as { name: string };
      const path = name === 'claude' ? '/home/dev/.claude/CLAUDE.md' : '/home/dev/.codex/AGENTS.md';
      const content = 'Use two spaces for indentation.\n';
      const diff = '--- on-disk\n+++ next-sync\n@@ -1 +1 @@\n-Use tabs for indentation.\n+Use two spaces for indentation.';
      return [{ path, format: 'block', content, next_content: content, differs: syncPending, diff: syncPending ? diff : '' }] satisfies T.InstructionPreview[];
    }
    const syncPreview = (): T.SyncResult => ({ fingerprint: JSON.stringify([syncPending, conflicts]), conflicts, actions: syncPending ? [
      { harness: 'claude', kind: 'update', item: 'instructions', name: 'style', dest: '/home/dev/.claude/CLAUDE.md', reason: null },
      { harness: 'codex', kind: 'conflict', item: 'skill', name: 'code-review', dest: '/home/dev/.codex/skills/code-review', reason: conflicts[0]?.reason ?? null },
    ] : [] });
    if (cmd === 'preview_sync') return syncPreview();
    if (cmd === 'sync_now') {
      if ((args as { fingerprint: string }).fingerprint !== syncPreview().fingerprint) return syncPreview();
      syncPending = false;
      return { conflicts } satisfies T.SyncResult;
    }
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === 'daemon_service') return delay({ default_socket: true, binary: '/home/dev/.cargo/bin/sk1ll', manager: 'systemd', available: true, active: false, restartable: false, unit: null });
    if (cmd === 'start_daemon') { setTimeout(() => { running = true; }, 1500); return delay('systemd', 600); }
    if (!running && cmd !== 'connection_info' && !cmd.startsWith('plugin:'))
      return Promise.reject({ message: 'connect ECONNREFUSED /run/user/1000/sk1ll/daemon.sock', status: null, unreachable: true });
    if (standalone && ['fleet', 'log', 'log_facets', 'undo', 'join_code', 'remove_machine'].includes(cmd)) return Promise.reject({ message: 'Unavailable in standalone mode', status: 409, unreachable: false });
    if (standalone && !status.has_origin && ['push', 'pull'].includes(cmd)) return Promise.reject({ message: 'Repository has no origin remote', status: 409, unreachable: false });
    switch (cmd) {
      case 'init': status.repo_ready = true; return delay(undefined);
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
        items.push({ item, id: newId(), in_baseline: false, draft: 'added', unpushed: false, description: r.description ?? null });
        draft.push({ item, change: 'added', paths: [path], fingerprint: `created-${r.name}` }); local.drafts.push(item);
        return delay(undefined);
      }
      case 'delete_item': case 'rename_item': {
        const r = a.request as T.RenameItemRequest;
        const item = `${r.kind}:${r.name}`;
        const entry = content.find(e => e.item === item && e.type === (r.kind === 'skill' ? 'personal' : 'rule'));
        if (!entry) return Promise.reject({ message: `No personal ${r.kind} named \`${r.name}\`` });
        const to = cmd === 'rename_item' ? `${r.kind}:${r.to}` : null;
        if (to && (!/^[a-zA-Z0-9][a-zA-Z0-9_.-]*$/.test(r.to) || r.to.includes('..'))) return Promise.reject({ message: `invalid skill name \`${r.to}\`` });
        if (to && content.some(e => e.item === to || (r.kind === 'skill' && e.type === 'import' && e.name === r.to))) return Promise.reject({ message: `name \`${r.to}\` is taken` });
        const path = (name: string) => r.kind === 'skill' ? `personal/${name}/SKILL.md` : `instructions/${name}.md`;
        const pending = draft.findIndex(d => d.item === item);
        // Deleting a never-committed item leaves nothing in the Draft.
        const added = pending >= 0 && draft[pending].change === 'added';
        if (pending >= 0) draft.splice(pending, 1);
        if (!added) draft.push({ item, change: 'deleted', paths: [path(r.name)], fingerprint: `deleted-${r.name}` });
        local.drafts = local.drafts.filter(d => d !== item); if (!added) local.drafts.push(item);
        const index = items.findIndex(i => i.item === item);
        const id = items[index]?.id ?? newId();
        content.splice(content.indexOf(entry), 1);
        if (!to) {
          if (added) items.splice(index, 1); else if (index >= 0) items[index].draft = 'deleted';
          return delay(undefined);
        }
        content.push({ ...entry, name: r.to, item: to, draft: 'added', in_baseline: false, path: `${status.repo}/${path(r.to).replace(/\/SKILL\.md$/, '')}` });
        if (index >= 0) { if (added) items.splice(index, 1); else items[index].draft = 'deleted'; }
        items.push({ item: to, id, in_baseline: false, draft: 'added', unpushed: false, description: entry.description });
        draft.push({ item: to, change: 'added', paths: [path(r.to)], fingerprint: `renamed-${r.to}` }); local.drafts.push(to);
        return delay({ item: to });
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
      case 'force_update_import':
      case 'update_import': {
        const entry = imports.imports.find(i => i.name === a.name);
        if (!entry) return Promise.reject({ message: 'Import no longer exists' });
        const update = imports.updates.find(i => i.name === a.name);
        if (a.expected && a.expected !== (update?.upstream ?? entry.locked))
          return Promise.reject({ status: 409, message: 'The upstream ref changed since the preview' });
        const old = entry.locked; entry.locked = update?.upstream ?? entry.locked;
        imports.updates = imports.updates.filter(i => i.name !== a.name);
        return delay({ name: entry.name, old_commit: old, new_commit: entry.locked ?? '', changed: old !== entry.locked });
      }
      case 'import_changes': {
        const update = imports.updates.find(i => i.name === a.name);
        if (!update || !update.locked) return Promise.reject({ status: 404, message: 'No upstream changes' });
        const upstream = String(a.upstream ?? update.upstream);
        return delay({ locked: update.locked, upstream, truncated: false, files: ['SKILL.md'],
          commits: [{ sha: upstream, date: '2026-11-19', subject: 'Update skill instructions' }] } satisfies T.ImportChanges);
      }
      case 'check_imports': imports.checked_at = Math.floor(Date.now() / 1000); return delay(imports);
      case 'status': return delay(status, 60);
      case 'fleet':
        local.items = items.flatMap(({ item, id, draft }) => id == null || draft === 'deleted' ? [] : [{ item, id }]);
        return delay({ baseline: '9f3c2a17d4e1', machines: empty ? machines.slice(0, 1) : machines });
      case 'items': return delay(items);
      case 'skills': return delay(structuredClone(content));
      case 'skill_tree': {
        const query = a.query as T.ContentQuery;
        return delay(query.item.startsWith('rule:') ? [{ path: `${query.item.slice(5)}.md`, kind: 'file', size: 120 }]
          : [{ path: 'SKILL.md', kind: 'file', size: 120 }, { path: 'references', kind: 'directory', size: 0 }, { path: 'references/example.md', kind: 'file', size: 50 }]);
      }
      case 'skill_file': {
        const query = a.query as T.ContentQuery & { path: string };
        const edited = query.hash ? undefined : editedFiles.get(JSON.stringify([query.item, query.path]));
        if (edited) return delay({ path: query.path, ...edited, binary: false, too_large: false, size: new TextEncoder().encode(edited.text).length });
        return delay({ fingerprint: query.hash || !content.some(entry => entry.item === query.item && (entry.type === 'personal' || entry.type === 'rule')) ? null : "mock-file", path: query.path, text: `---\nname: ${query.item.split(':')[1]}\ndescription: ${query.hash === 'sha256:two' ? 'Collect detailed notes.' : 'Work with this Skill.'}\n---\n# Instructions\n\nUse **careful review** before making changes.\n\n- Read the files.\n- Check the result.`, binary: false, too_large: false, size: 120 });
      }
      case 'write_skill_file': {
        const request = a.request as T.ContentQuery & { path: string; expected: string; content: string };
        const key = JSON.stringify([request.item, request.path]);
        if (request.expected !== (editedFiles.get(key)?.fingerprint ?? 'mock-file')) {
          return Promise.reject({ status: 409, message: 'File changed on disk' });
        }
        editedFiles.set(key, { text: request.content, fingerprint: `file-${++fileRevision}` });
        return delay(undefined);
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
        if (!items.some(item => item.item === entry.item)) items.push({ item: entry.item, id: newId(), in_baseline: false, draft: entry.draft, unpushed: false, description: entry.description });
        draft.push({ item: entry.item, change: entry.draft, paths: [`personal/${name}/SKILL.md`], fingerprint: 'converted' });
        local.drafts.push(entry.item);
        return delay(undefined);
      }
      case 'assignment': {
        const registration = machines.find(m => m.id === a.machine)?.registration;
        return delay({ ...assignments[a.machine as string] ?? { revision: 0, harnesses: {} }, ...registration && { registration } });
      }
      case 'set_harness_assignment': {
        const snapshot = assignments[status.machine_id];
        const enabled = Boolean(a.enabled);
        const h = snapshot.harnesses[a.harness as string] ??= harness();
        h.new_skills_default = enabled;
        h.new_rules_default = enabled;
        for (const toggles of [h.skills, h.rules]) {
          for (const id of Object.keys(toggles)) toggles[id] = enabled;
        }
        snapshot.revision++;
        const { revision, ...assignment } = snapshot;
        return delay({ revision, assignment: structuredClone(assignment), entries: [] });
      }
      case 'orphans': return delay({ orphans: orphanToggles() });
      case 'prune_orphans': {
        const self = machines.find(machine => machine.id === status.machine_id)?.scope === 'self';
        const orphans = orphanToggles().filter(orphan => !self || orphan.machine === status.machine_id);
        const changed = new Set<string>();
        const entries = orphans.map((orphan, i): T.LogEntry => {
          const h = assignments[orphan.machine].harnesses[orphan.harness];
          const toggles = orphan.kind === 'skill' ? h.skills : h.rules;
          const before = toggles[orphan.item_id];
          delete toggles[orphan.item_id];
          changed.add(orphan.machine);
          return { ...orphan, id: (log[0]?.id ?? 0) + i + 1, at: now, author: status.machine_id,
            before, after: null, cause: { type: 'orphan_pruned' } };
        });
        for (const machine of changed) assignments[machine].revision++;
        log.unshift(...[...entries].reverse());
        return delay({ entries });
      }
      case 'preview_assignment_copy':
      case 'copy_machine_assignment': {
        const copy = a.copy as T.MachineAssignmentCopy;
        const target = assignments[a.machine as string], source = assignments[copy.source];
        if (!target || !source) return Promise.reject({ status: 404, message: 'Machine no longer exists' });
        const stamp = (id: string, snapshot: T.AssignmentSnapshot) => ({
          revision: snapshot.revision, registration: machines.find(machine => machine.id === id)?.registration,
        });
        const current = stamp(a.machine as string, target), from = stamp(copy.source, source);
        const matches = (expected: T.Expected | undefined, actual: T.Expected) =>
          !expected || (expected.revision === actual.revision && (!expected.registration || expected.registration === actual.registration));
        if (!matches(copy.expected, current) || !matches(copy.source_expected, from))
          return Promise.reject({ status: 409, message: 'Assignment changed since the preview' });
        const next = structuredClone(target.harnesses);
        const entries: T.LogEntry[] = [];
        const record = (harness: string, kind: T.ToggleKind, item_id: string | null, before: boolean | null, after: boolean | null) => {
          if (before === after) return;
          entries.push({ id: Math.max(0, ...log.map(entry => entry.id)) + entries.length + 1,
            at: Math.floor(Date.now() / 1000), author: status.machine_id, machine: a.machine as string,
            harness, kind, item_id, name: items.find(item => item.id === item_id)?.item.split(':')[1] ?? null, before, after, cause: { type: 'change' } });
        };
        for (const mapping of copy.harnesses ?? []) {
          const src = source.harnesses[mapping.from], dest = next[mapping.to] ??= harness();
          if (!src) continue;
          for (const kind of ['skills', 'rules'] as const) {
            const toggle = kind === 'skills' ? 'skill' : 'rule';
            if (copy.mode === 'replace') {
              const key = kind === 'skills' ? 'new_skills_default' : 'new_rules_default';
              record(mapping.to, key, null, dest[key], src[key]);
              dest[key] = src[key];
              for (const name of Object.keys(dest[kind])) {
                if (!Object.hasOwn(src[kind], name)) {
                  record(mapping.to, toggle, name, dest[kind][name], null);
                  delete dest[kind][name];
                }
              }
            }
            for (const [name, enabled] of Object.entries(src[kind])) {
              if (copy.mode === 'merge' && Object.hasOwn(dest[kind], name)) continue;
              record(mapping.to, toggle, name, dest[kind][name] ?? null, enabled);
              dest[kind][name] = enabled;
            }
          }
        }
        if (cmd === 'preview_assignment_copy') return delay({ count: entries.length, target: current, source: from });
        target.harnesses = next;
        if (entries.length) {
          target.revision++;
          log.unshift(...entries.slice().reverse());
        }
        return delay({ assignment: { harnesses: next }, revision: target.revision, entries });
      }
      case 'set_assignment': {
        const snapshot = assignments[a.machine as string];
        const { changes, expected } = a.changes as T.AssignmentChanges;
        const registration = machines.find(m => m.id === a.machine)?.registration;
        if (expected && (expected.revision !== snapshot.revision || (expected.registration && expected.registration !== registration))) {
          return Promise.reject({ status: 409, message: `the assignment of \`${a.machine}\` changed since revision ${expected.revision} (now ${snapshot.revision}); reload it` });
        }
        for (const change of changes) {
          const h = snapshot.harnesses[change.harness] ??= harness();
          if (change.kind === 'skill' || change.kind === 'rule') {
            const map = change.kind === 'skill' ? h.skills : h.rules;
            if (change.enabled === null) delete map[change.item_id!]; else map[change.item_id!] = change.enabled;
          } else h[change.kind] = !!change.enabled;
        }
        snapshot.revision++;
        const { revision, ...assignment } = snapshot;
        return delay({ revision, assignment: structuredClone(assignment), entries: [] }, 400);
      }
      case 'draft': return delay(draft);
      case 'diff': return delay({ item: a.item, diff: diffs[a.item as string] ?? '' }, 50);
      case 'commit_message': return delay({ items: a.items, message: `Commit ${(a.items as string[]).join(', ')}` });
      case 'commit': {
        const request = a.request as T.CommitRequest;
        if (standalone) {
          for (let i = draft.length - 1; i >= 0; i--) if (request.items.includes(draft[i].item)) draft.splice(i, 1);
          local.drafts = local.drafts.filter(item => !request.items.includes(item));
          local.unpushed = [...new Set([...local.unpushed, ...request.items])];
          content.filter(entry => entry.item && request.items.includes(entry.item)).forEach(entry => { entry.draft = null; entry.in_baseline = true; });
          items.filter(entry => request.items.includes(entry.item)).forEach(entry => { entry.draft = null; entry.unpushed = true; entry.in_baseline = true; });
        }
        return delay({ sha: '7d1e9a0b3c5f8e2d', items: request.items, message: request.message ?? '', pushed: !standalone, push_error: null }, 600);
      }
      case 'restore': return delay({ discarded: [] });
      case 'pull': return delay({ state: 'up_to_date' }, 500);
      case 'conflicts': return delay(conflicts);
      case 'recovered': return delay(recovered);
      case 'harnesses': return delay(harnesses);
      case 'setup_harnesses': return delay(setupHarnesses);
      case 'set_setup_harness': {
        const choice = setupHarnesses.find(h => h.name === a.name);
        if (choice) choice.enabled = a.enabled as boolean;
        return delay(setupHarnesses);
      }
      case 'notifications_enabled': return delay(notificationsEnabled);
      case 'set_notifications': notificationsEnabled = Boolean(a.enabled); return delay(notificationsEnabled);
      case 'harness_settings': return delay({ path: machineConfig, harnesses: setupHarnesses });
      case 'preview_harness': return delay(harnessPreview((a.request as T.HarnessChangeRequest).change), 400);
      case 'apply_harness': {
        const { change, fingerprint } = a.request as T.HarnessChangeRequest;
        if (fingerprint !== harnessPreview(change).fingerprint)
          return Promise.reject({ status: 409, message: 'machine.toml changed since the preview — preview again' });
        const index = setupHarnesses.findIndex(h => h.name === change.name);
        if (change.op === 'set') {
          if (index < 0) setupHarnesses.push({ name: change.name, installed: true, enabled: true, preset: false, configured: true, harness: change.harness });
          else Object.assign(setupHarnesses[index], { harness: change.harness, configured: true });
        } else if (change.op === 'remove' && index >= 0 && !setupHarnesses[index].preset) setupHarnesses.splice(index, 1);
        else if (index >= 0) Object.assign(setupHarnesses[index], change.op === 'remove' ? { configured: false, enabled: setupHarnesses[index].installed } : { enabled: change.op === 'enable', configured: true, available: change.op === 'enable' ? false : setupHarnesses[index].available });
        return delay({ harnesses: setupHarnesses, errors: [] }, 600);
      }
      case 'open_fleet_deployment_docs':
      case 'open_machine_config': return delay(undefined);
      case 'restart_daemon': return delay(undefined);
      case 'imports': return delay(imports);
      // A tree/blob link resolves one Skill; a repository link resolves several.
      case 'resolve_import': {
        const request = a.request as T.ResolveRequest;
        const single = /\/(tree|blob)\//.test(request.link);
        return delay({ source: 'github:example/skills', git_ref: request.ref ?? 'main', commit: 'c0ffee1234567890',
          candidates: (single ? ['pdf'] : ['pdf', 'writing']).map(name => ({ path: request.path ?? `skills/${name}`, total_bytes: 2048,
            warnings: name === 'pdf' ? [{ path: 'scripts/check.py', kinds: ['script', 'executable'] }] : [], default_name: name, description: `Tools for ${name}.`, taken: items.some(i => i.item === `skill:${name}`) })) });
      }
      case 'add_import': {
        const request = a.request as T.AddImportRequest;
        if (items.some(i => i.item === `skill:${request.name}`))
          return Promise.reject({ message: 'Import name already exists', status: 409, unreachable: false });
        imports.imports.push({ name: request.name, source: request.source, ref: request.ref, path: request.path, locked: request.commit ?? 'c0ffee1234567890' });
        content.push({ name: request.name, item: `skill:${request.name}`, type: 'import', description: null, draft: 'added', in_baseline: false, path: `${status.repo}/imported/${request.name}`, enabled_harnesses: [], clashes_with_repo: false, variants: [] });
        items.push({ item: `skill:${request.name}`, id: newId(), in_baseline: false, draft: 'added', unpushed: false, description: null });
        draft.push({ item: `skill:${request.name}`, change: 'added', paths: [`imported/${request.name}/SKILL.md`], fingerprint: 'new' });
        return delay({ name: request.name, old_commit: null, new_commit: 'c0ffee1234567890', changed: true });
      }
      case 'log': {
        const q = (a.query ?? {}) as T.LogQuery;
        const term = q.item?.toLowerCase() ?? '';
        const rows = log.filter(e => (!q.machine || e.machine === q.machine) && (!q.harness || e.harness === q.harness)
          && (!q.author || e.author === q.author) && (q.before === undefined || e.id < q.before)
          && (`${e.kind}:${e.name ?? ''}`.toLowerCase().includes(term) || e.item_id?.toLowerCase().includes(term)));
        return delay(rows.slice(0, Math.min(Math.max(q.limit ?? 100, 1), 1000)));
      }
      case 'log_facets': {
        const distinct = (key: 'machine' | 'harness' | 'author') => [...new Set(log.map(e => e[key]))].sort();
        return delay({ machines: distinct('machine'), harnesses: distinct('harness'), authors: distinct('author') });
      }
      case 'join_code': return delay({ code: 'sk1ll-join-7Q2M-XK9P-44RD-ZT8W', expires_at: Math.floor(Date.now() / 1000) + 86400, scope: (a as { scope?: T.Scope }).scope ?? 'fleet' });
      case 'connection_info': return delay('/run/user/1000/sk1ll/daemon.sock', 10);
      case 'adopt': case 'overwrite': case 'adopt_displaced': case 'discard_displaced': case 'undo':
      case 'remove_machine': return delay(undefined, 500);
      case 'join':
        if (standalone && content.length) return Promise.reject({ message: 'Cannot join a Fleet while the local repo has content.', status: 409, unreachable: false });
        standalone = false; status.mode = 'joined'; status.connection = 'connected'; status.has_origin = true; status.repo_ready = true;
        status.server_url = (a.request as T.JoinApiRequest).server_url;
        return delay({ machine_id: (a.request as T.JoinApiRequest).machine_id ?? 'laptop', repo: '/home/user/.local/share/sk1ll/repo', setup: 'cloned', harnesses: ['claude', 'codex'], revision: 1 }, 500);
      default: return Promise.reject({ message: `Unhandled mock command: ${cmd}` });
    }
  }, { shouldMockEvents: true });
}
