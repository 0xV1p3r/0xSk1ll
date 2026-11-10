import { useState } from 'react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { parseDocument } from 'yaml';
import { Button } from '@/components/ui/button';
import { Details, Notice } from './shared';
import type { FileContent } from './types';

function frontmatter(text: string) {
  const match = /^(?:\uFEFF)?---\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)(?:\r?\n|$)/.exec(text);
  if (!match) return { body: text, rows: [] as [string, string][] };
  try {
    const doc = parseDocument(match[1], { uniqueKeys: true });
    const data: unknown = doc.toJS({ maxAliasCount: 50 });
    if (doc.errors.length || !data || typeof data !== 'object' || Array.isArray(data)) return { body: text, rows: [] as [string, string][] };
    return { body: text.slice(match[0].length), rows: Object.entries(data).map(([key, value]): [string, string] =>
      [key, typeof value === 'string' ? value : JSON.stringify(value)]) };
  } catch { return { body: text, rows: [] as [string, string][] }; }
}

export function SkillFile({ file }: { file: FileContent }) {
  const [raw, setRaw] = useState(false);
  if (file.too_large) return <Notice title="File too large">This file exceeds the 1 MiB preview limit.</Notice>;
  if (file.binary || file.text === null) return <Notice title="Binary file">This file has no text preview.</Notice>;
  const markdown = /\.md$/i.test(file.path);
  const { body, rows } = markdown ? frontmatter(file.text) : { body: file.text, rows: [] };
  return <div className="flex min-w-0 flex-col gap-4 rounded-xl border border-border bg-card p-4">
    <div className="-mx-4 -mt-4 flex flex-wrap items-center justify-between gap-2 border-b border-border bg-muted/40 px-4 py-3"><h3 className="break-all font-mono text-xs">{file.path}</h3>
      {markdown && <div role="group" aria-label="Markdown view" className="flex gap-1">
        <Button size="default" variant={raw ? 'ghost' : 'secondary'} aria-pressed={!raw} onClick={() => setRaw(false)}>Rendered</Button>
        <Button size="default" variant={raw ? 'secondary' : 'ghost'} aria-pressed={raw} onClick={() => setRaw(true)}>Raw</Button>
      </div>}
    </div>
    {!!rows.length && <section aria-label="Frontmatter" className="rounded-md border bg-muted/30 p-3"><Details rows={rows} /></section>}
    {markdown && !raw ? <article className="skill-markdown"><Markdown remarkPlugins={[remarkGfm]} skipHtml
      components={{ a: ({ children, ...props }) => <a {...props} target="_blank" rel="noreferrer">{children}</a>,
        img: ({ alt }) => <span>[Image: {alt || 'No description'}]</span> }}>{body}</Markdown></article>
      : <pre className="overflow-x-auto rounded-md border bg-muted/30 p-3 font-mono text-xs whitespace-pre-wrap break-words">{body}</pre>}
  </div>;
}
