import { describe, expect, it } from 'vitest';
import { applyToggle, cellState, emptyHarness, everythingOn, headline, insecureHttp, itemStateLabel, mergeAssignments, statusLabels, statusOrder, statusTone } from './model';
import type { Assignment, MachineReport } from './types';

const report: MachineReport = { baseline: 'a', drafts: [], unpushed: [], conflicts: [], harnesses: [], blocked: false, clashes: [], behind: false };
describe('Machine status and badges', () => {
  it('uses the glossary severity ordering even when every condition applies', () => {
    const busy = { ...report, blocked: true, behind: true, drafts: ['skill:pdf'], conflicts: [{ item: 'skill:pdf', harness: 'claude' }] };
    expect(headline(true, busy, 'b')).toBe('offline');
    expect(headline(false, busy, 'b')).toBe('blocked');
    expect(headline(false, { ...busy, blocked: false }, 'b')).toBe('conflict');
    expect(headline(false, { ...busy, blocked: false, conflicts: [] }, 'b')).toBe('behind');
    expect(headline(false, { ...report, drafts: ['skill:pdf'] }, 'a')).toBe('drafting');
    expect(headline(false, report, 'a')).toBe('in_sync');
    expect(statusOrder.map(s => statusLabels[s])).toEqual(['Offline', 'Blocked', 'Conflict', 'Behind', 'Drafting', 'In sync']);
    expect(statusOrder.map(s => statusTone[s])).toEqual(['secondary', 'destructive', 'warning', 'info', 'drafting', 'success']);
  });
  it('handles absent reports and a Baseline that has not been created', () => {
    expect(headline(false, null, 'a')).toBe('behind');
    expect(headline(false, null, null)).toBe('in_sync');
    expect(headline(false, { ...report, behind: true }, 'a')).toBe('behind');
  });
});
describe('Assignment cells', () => {
  const assignment: Assignment = { harnesses: { claude: { ...emptyHarness(), new_skills_default: false, skills: { pdf: true, git: false } } } };
  it('distinguishes explicit on and off from inherited state', () => {
    expect(cellState(assignment, 'claude', 'skill:pdf')).toEqual({ enabled: true, explicit: true });
    expect(cellState(assignment, 'claude', 'skill:git')).toEqual({ enabled: false, explicit: true });
    expect(cellState(assignment, 'claude', 'skill:new')).toEqual({ enabled: false, explicit: false });
    expect(cellState(assignment, 'claude', 'rule:git')).toEqual({ enabled: true, explicit: false });
    expect(cellState(assignment, 'codex', 'skill:new')).toEqual({ enabled: true, explicit: false });
  });
  it('removes explicit toggles when following the default again', () => {
    const next = applyToggle(assignment, { harness: 'claude', kind: 'skill', name: 'pdf', enabled: null });
    expect(cellState(next, 'claude', 'skill:pdf')).toEqual({ enabled: false, explicit: false });
    expect(assignment.harnesses.claude.skills.pdf).toBe(true);
  });
  it('leaves default materialization to the Server', () => {
    const next = applyToggle(assignment, { harness: 'claude', kind: 'new_skills_default', enabled: true });
    expect(next).toEqual(assignment);
  });
});
describe('revisioned Assignment snapshots', () => {
  const assignment: Assignment = { harnesses: {} };
  it('rejects lower revisions per machine and accepts newer snapshots', () => {
    const a = { assignment, revision: 4 }, b = { assignment, revision: 2 };
    const merged = mergeAssignments({ a, b }, { a: { assignment, revision: 3 }, b: { assignment, revision: 5 } });
    expect(merged.a).toBe(a);
    expect(merged.b.revision).toBe(5);
    expect(mergeAssignments(merged, { b }).a).toBe(a);
  });
  it('takes a snapshot requested after the cached one arrived, whatever its revision', () => {
    const cached = { assignment, revision: 7, sent: 1, received: 2 };
    expect(mergeAssignments({ a: cached }, { a: { assignment, revision: 1, sent: 3, received: 4 } }).a.revision).toBe(1);
    // Overlapping requests: the later revision wins.
    expect(mergeAssignments({ a: cached }, { a: { assignment, revision: 1, sent: 1.5, received: 4 } }).a).toBe(cached);
  });
  it('keeps prototype names as ordinary Machine ids', () => {
    const merged = mergeAssignments({}, Object.fromEntries([['__proto__', { assignment, revision: 1 }], ['constructor', { assignment, revision: 2 }]]));
    expect(Object.getPrototypeOf(merged)).toBe(Object.prototype);
    expect(Object.keys(merged).sort()).toEqual(['__proto__', 'constructor']);
  });
  it('labels unpushed content even when the item also exists in Baseline', () => {
    expect(itemStateLabel({ in_baseline: true, unpushed: true })).toBe('Unpushed');
    expect(itemStateLabel({ in_baseline: false, unpushed: false })).toBe('Draft only');
    expect(itemStateLabel({ in_baseline: true, unpushed: false })).toBeUndefined();
  });
});

describe('first-run setup', () => {
  it('turns on defaults first, then every Baseline item, in each Harness', () => {
    const items = [
      { item: 'skill:pdf', in_baseline: true, draft: null, unpushed: false, description: null },
      { item: 'rule:git', in_baseline: true, draft: null, unpushed: false, description: null },
      { item: 'skill:local', in_baseline: false, draft: 'added' as const, unpushed: false, description: null },
    ];
    expect(everythingOn(['claude'], items)).toEqual([
      { harness: 'claude', kind: 'new_skills_default', enabled: true }, { harness: 'claude', kind: 'new_rules_default', enabled: true },
      { harness: 'claude', kind: 'skill', name: 'pdf', enabled: true }, { harness: 'claude', kind: 'rule', name: 'git', enabled: true },
    ]);
    expect(everythingOn([], items)).toEqual([]);
  });
  it('warns about plain HTTP except to this computer', () => {
    expect(insecureHttp('http://skills.example.com')).toBe(true);
    expect(insecureHttp('http://10.0.0.5:7878')).toBe(true);
    for (const url of ['https://skills.example.com', 'http://localhost:7878', 'http://127.0.0.1:7878', 'http://[::1]:7878', 'not a url', ''])
      expect(insecureHttp(url)).toBe(false);
  });
});
