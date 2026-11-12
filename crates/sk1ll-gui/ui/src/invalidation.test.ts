import { describe, expect, it } from 'vitest';
import { allTopics, eventTopics } from './invalidation';

describe('background invalidation', () => {
  it('routes draft changes to local content and imports, without fleet', () => {
    expect(eventTopics('draft')).toEqual(['status', 'content', 'imports']);
  });
  it('routes status changes without imports or fleet', () => {
    expect(eventTopics('status')).toEqual(['status', 'content']);
  });
  it('keeps fleet changes away from open files and imports', () => {
    expect(eventTopics('fleet')).toEqual(['fleet', 'history']);
  });
  it('refreshes every topic after daemon reconnection or loss', () => {
    expect(eventTopics('daemon-connection')).toEqual(allTopics);
  });
  it('refreshes fleet assignments when a local assignment is applied', () => {
    expect(eventTopics('assignment')).toEqual(['status', 'content', 'fleet']);
  });
});
