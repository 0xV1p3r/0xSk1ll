import { useEffect, useRef, useState } from 'react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { parseDocument } from 'yaml';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { Field, FieldLabel } from '@/components/ui/field';
import { api, errorStatus } from './api';
import { useMutation } from './hooks';
import { Details, DiffView, ErrorNotice, Notice } from './shared';
import { unifiedDiff } from './textDiff';
import type { ContentQuery, FileContent } from './types';

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

export type FileEditState = 'clean' | 'dirty' | 'saving';

export function canLeaveFile(state: FileEditState) {
  return state === 'clean' || (state === 'dirty' && window.confirm('Discard unsaved changes?'));
}

type FileEditorProps = {
  query: ContentQuery;
  writable: boolean;
  setEditing: (editing: boolean) => void;
  changed: () => void;
  guard: (state: FileEditState) => void;
  reload: (file: FileContent) => void;
};

function FileEditor({ file, query, changed, guard, reload, exit, setEditing: setBrowserEditing }: FileEditorProps & { file: FileContent; exit: () => void }) {
  const [base, setBase] = useState(file);
  const [text, setText] = useState(file.text ?? '');
  const [disk, setDisk] = useState<FileContent>();
  const [conflict, setConflict] = useState(false);
  const mutation = useMutation();
  const dirty = text !== base.text;
  const request = { ...query, path: file.path };

  useEffect(() => {
    setBrowserEditing(true);
    return () => setBrowserEditing(false);
  }, [setBrowserEditing]);

  useEffect(() => {
    guard(mutation.pending ? 'saving' : dirty ? 'dirty' : 'clean');
    return () => guard('clean');
  }, [dirty, guard, mutation.pending]);

  useEffect(() => {
    if (!dirty && !mutation.pending) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [dirty, mutation.pending]);

  const readDisk = async () => {
    setDisk(undefined);
    setDisk(await api.skillFile(request));
  };
  const save = (expected: string) => void mutation.run(async () => {
    try {
      await api.writeSkillFile({ ...request, expected, content: text });
    } catch (error) {
      if (errorStatus(error) !== 409) throw error;
      setConflict(true);
      await readDisk();
      return false;
    }
    return true;
  }, saved => {
    if (saved) {
      guard('clean');
      reload({ ...base, text, fingerprint: null, size: new TextEncoder().encode(text).length });
      exit();
      changed();
    }
  });
  const reset = () => {
    if (dirty && !window.confirm('Discard unsaved changes?')) return;
    if (disk) reload(disk);
    guard('clean');
    exit();
  };
  const applyDisk = (latest: FileContent) => {
    reload(latest);
    if (latest.text === null || !latest.fingerprint) { guard('clean'); exit(); return; }
    setBase(latest);
    setText(latest.text);
    setDisk(undefined);
    setConflict(false);
  };
  const saveCurrent = () => {
    if (dirty && !conflict && !mutation.pending && base.fingerprint) save(base.fingerprint);
  };
  return <form className="flex flex-col gap-3" onSubmit={event => { event.preventDefault(); saveCurrent(); }}
    onKeyDown={event => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
        event.preventDefault();
        saveCurrent();
      }
    }}>
    <Field><FieldLabel htmlFor="skill-file-content">{file.path}</FieldLabel>
      <Textarea id="skill-file-content" autoFocus className="min-h-64 font-mono" value={text} disabled={mutation.pending}
        onChange={event => setText(event.target.value)} />
    </Field>
    <div className="flex gap-2">
      <Button type="submit" disabled={!dirty || conflict || mutation.pending}>Save</Button>
      <Button type="button" variant="outline" disabled={mutation.pending} onClick={reset}>Cancel</Button>
    </div>
    {conflict && <Notice title="File changed on disk">
      {disk?.text != null && <><p>Changes that Overwrite will apply to the file on disk</p>
        <DiffView diff={unifiedDiff(file.path, disk.text, text)} label="Changes on disk" empty={disk.text === text ? 'No differences.' : 'The final newline differs.'} className="max-h-64" /></>}
      <div className="flex gap-2">
        <Button type="button" variant="outline" disabled={mutation.pending} onClick={() => {
          if (disk) applyDisk(disk);
          else void mutation.run(() => api.skillFile(request), applyDisk);
        }}>Reload</Button>
        {disk?.fingerprint && <Button type="button" variant="destructive" disabled={mutation.pending}
          onClick={() => { if (disk.fingerprint) save(disk.fingerprint); }}>Overwrite</Button>}
      </div>
    </Notice>}
    <ErrorNotice error={mutation.error} />
  </form>;
}

export function SkillFile({ file, editor }: { file: FileContent; editor?: FileEditorProps }) {
  const [editing, setEditing] = useState(false);
  const [raw, setRaw] = useState(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const restoreFocus = useRef(false);
  useEffect(() => {
    if (!editing && restoreFocus.current) {
      restoreFocus.current = false;
      (heading.current ?? document.getElementById('main'))?.focus();
    }
  }, [editing]);
  if (editing && editor) return <FileEditor file={file} {...editor} exit={() => { restoreFocus.current = true; setEditing(false); }} />;
  if (file.too_large) return <Notice title="File too large">This file exceeds the 1 MiB preview limit.</Notice>;
  if (file.binary || file.text === null) return <Notice title="Binary file">This file has no text preview.</Notice>;
  const markdown = /\.md$/i.test(file.path);
  const { body, rows } = markdown ? frontmatter(file.text) : { body: file.text, rows: [] };
  return <div className="flex min-w-0 flex-col gap-4 rounded-xl border border-border bg-card p-4">
    <div className="-mx-4 -mt-4 flex flex-wrap items-center justify-between gap-2 border-b border-border bg-muted/40 px-4 py-3"><h3 ref={heading} tabIndex={-1} className="break-all font-mono text-xs">{file.path}</h3>
      <div className="flex flex-wrap gap-1">
      {editor?.writable && file.fingerprint && <Button variant="outline" onClick={() => setEditing(true)}>Edit</Button>}
      {markdown && <div role="group" aria-label="Markdown view" className="flex gap-1">
        <Button size="default" variant={raw ? 'ghost' : 'secondary'} aria-pressed={!raw} onClick={() => setRaw(false)}>Rendered</Button>
        <Button size="default" variant={raw ? 'secondary' : 'ghost'} aria-pressed={raw} onClick={() => setRaw(true)}>Raw</Button>
      </div>}
      </div>
    </div>
    {!!rows.length && <section aria-label="Frontmatter" className="rounded-md border bg-muted/30 p-3"><Details rows={rows} /></section>}
    {markdown && !raw ? <article className="skill-markdown"><Markdown remarkPlugins={[remarkGfm]} skipHtml
      components={{ a: ({ children, ...props }) => <a {...props} target="_blank" rel="noreferrer">{children}</a>,
        img: ({ alt }) => <span>[Image: {alt || 'No description'}]</span> }}>{body}</Markdown></article>
      : <pre className="overflow-x-auto rounded-md border bg-muted/30 p-3 font-mono text-xs whitespace-pre-wrap break-words">{body}</pre>}
  </div>;
}
