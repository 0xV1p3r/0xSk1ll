import { useEffect, useRef, useState } from 'react';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { cn } from '@/lib/utils';
export type Command = { id: string; label: string; select: () => void };
export function CommandPalette({ open, onOpenChange, commands }: { open: boolean; onOpenChange: (open: boolean) => void; commands: Command[] }) {
  const navigating = useRef(false);
  const previousFocus = useRef<HTMLElement | null>(null);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const matches = commands.filter(command => command.label.toLowerCase().includes(query.toLowerCase()));
  useEffect(() => { if (open) { navigating.current = false; setQuery(''); setActive(0); } }, [open]);
  const select = (command: Command) => { navigating.current = true; onOpenChange(false); command.select(); };
  return <Dialog open={open} onOpenChange={onOpenChange}><DialogContent initialFocus={() => { previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null; return true; }} finalFocus={() => navigating.current ? false : previousFocus.current}>
    <DialogHeader><DialogTitle>Go to…</DialogTitle><DialogDescription>Search screens, Skills, and Rules.</DialogDescription></DialogHeader>
    <Input aria-label="Search commands" role="combobox" aria-autocomplete="list" aria-expanded="true" aria-controls="command-results"
      aria-activedescendant={matches[active]?.id} value={query} onChange={event => { setQuery(event.target.value); setActive(0); }}
      onKeyDown={event => { if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); setActive(index => matches.length ? (index + (event.key === 'ArrowDown' ? 1 : -1) + matches.length) % matches.length : 0); }
        if (event.key === 'Enter' && matches[active]) { event.preventDefault(); select(matches[active]); } }} />
    <div id="command-results" role="listbox" aria-label="Commands" className="flex max-h-80 flex-col overflow-auto">
      {matches.map((command, index) => <div key={command.id} id={command.id} role="option" aria-selected={index === active}
        className={cn('cursor-pointer rounded-md px-3 py-2 text-sm', index === active && 'bg-accent text-accent-foreground')}
        onMouseMove={() => setActive(index)} onClick={() => select(command)}>{command.label}</div>)}
      {!matches.length && <p role="status" className="p-3 text-sm text-muted-foreground">No matching commands</p>}
    </div>
  </DialogContent></Dialog>;
}
