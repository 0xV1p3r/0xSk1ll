import { useCallback, useEffect, useRef, useState } from 'react';
import { errorMessage } from './api';

const replace = <T,>(_previous: T | undefined, next: T) => next;
export function useResource<T>(load: () => Promise<T>, revision: number, merge = replace<T>, enabled = true) {
  const [data, setData] = useState<T>();
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const [updatedAt, setUpdatedAt] = useState<number>();
  const generation = useRef(0);
  const refresh = useCallback(async () => {
    const current = ++generation.current;
    setLoading(true);
    try {
      const value = await load();
      if (current === generation.current) { setData(previous => merge(previous, value)); setError(''); setUpdatedAt(Date.now() / 1000); }
    } catch (e) { if (current === generation.current) setError(errorMessage(e)); }
    finally { if (current === generation.current) setLoading(false); }
  }, [load, merge]);
  useEffect(() => {
    const counter = generation;
    if (enabled) void refresh();
    return () => { counter.current++; };
  }, [refresh, revision, enabled]);
  return { data, setData, error, loading, refresh, updatedAt };
}
export function useMutation(onSuccess?: () => void) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState('');
  const lock = useRef(false);
  const run = async <T,>(action: () => Promise<T>, done?: (value: T) => void) => {
    if (lock.current) return;
    lock.current = true;
    setPending(true); setError('');
    try { const value = await action(); done?.(value); onSuccess?.(); }
    catch (e) { setError(errorMessage(e)); }
    finally { lock.current = false; setPending(false); }
  };
  return { pending, error, run };
}
