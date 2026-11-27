// Harness kinds: the preset a Harness is, or is an instance of (a second config folder of the same tool).
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { HarnessView, MachineReport } from './types';

export type ReportedHarness = MachineReport['harnesses'][number];

// The tool each built-in preset is for, by kind. A Map, not an object: a kind comes from other Machines' reports, and
// one such as `constructor` must not find an inherited property.
const toolNames = new Map(Object.entries({
  claude: 'Claude Code', codex: 'Codex', opencode: 'OpenCode', gemini: 'Gemini CLI', copilot: 'Copilot CLI',
  cursor: 'Cursor', amp: 'Amp', kiro: 'Kiro', cline: 'Cline', roo: 'Roo Code',
}));
// An unknown kind (a newer sk1ll's preset, say) reads as itself.
export const toolName = (kind: string) => toolNames.get(kind) ?? kind;

// The kind of a Harness that isn't the preset of its own name: a preset instance, or a renamed preset.
export const instanceKind = (h: { name: string; kind: string | null }) => h.kind && h.kind !== h.name ? h.kind : null;

// A small badge naming the tool of a preset instance; nothing for a preset or a custom Harness.
export function HarnessKindBadge({ harness, className }: { harness: { name: string; kind: string | null }; className?: string }) {
  const kind = instanceKind(harness);
  if (!kind) return null;
  return <Badge variant="outline" className={cn('h-4 px-1.5 font-sans text-[10px] font-normal text-muted-foreground', className)} title={`An instance of the ${kind} preset`}>{toolName(kind)}</Badge>;
}

// A Harness name from a config folder's name: `~/.claude_2` → `claude_2`, then `_2`, `_3`… until it is free.
export function suggestName(home: string, taken: string[]): string {
  const base = home.replace(/\/+$/, '').split('/').pop() ?? '';
  const name = base.toLowerCase().replace(/[^a-z0-9_-]+/g, '_').replace(/^[_-]+|[_-]+$/g, '').slice(0, 60);
  if (!name) return '';
  if (!taken.includes(name)) return name;
  for (let n = 2; ; n++) if (!taken.includes(`${name}_${n}`)) return `${name}_${n}`;
}

// The preset's settings as this Machine has them, moved to `home`: each path under the preset's own folder moves with
// it; the Daemon writes into machine.toml only what differs from the preset's own layout there.
export function rehome(preset: HarnessView['harness'], home: string): HarnessView['harness'] {
  const from = preset.home ?? '';
  const move = (path: string | null) => !from || !path ? path : path === from ? home : path.startsWith(`${from}/`) ? home + path.slice(from.length) : path;
  return { ...preset, home, skills: move(preset.skills), instructions: move(preset.instructions) };
}
