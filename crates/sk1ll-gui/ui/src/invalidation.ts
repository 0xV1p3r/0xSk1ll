export type Topic = 'status' | 'content' | 'imports' | 'fleet' | 'history';
export const allTopics: Topic[] = ['status', 'content', 'imports', 'fleet', 'history'];
// Content includes inventories, draft, recovery, conflicts, editors and local history.
// Status deliberately includes content because reports can introduce conflicts.
export function eventTopics(event: string): Topic[] {
  if (event === 'daemon-connection') return allTopics;
  if (event === 'imports') return ['imports'];
  if (event === 'fleet') return ['fleet', 'history'];
  // A Draft change can add, update or remove an Import in the manifest.
  if (event === 'draft') return ['status', 'content', 'imports'];
  if (event === 'assignment') return ['status', 'content', 'fleet'];
  return ['status', 'content'];
}
