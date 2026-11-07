import { useCallback, useEffect, useRef, useState } from 'react';
import { errorMessage } from './api';

const replace = <T,>(_previous: T | undefined, next: T) => next;
export function useResource<T>(load: () => Promise<T>, revision: number, merge = replace<T>) {
  const [data, setData] = useState<T>();
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const generation = useRef(0);
  const refresh = useCallback(async () => {
    const current = ++generation.current;
    setLoading(true);
    try {
      const value = await load();
      if (current === generation.current) { setData(previous => merge(previous, value)); setError(''); }
    } catch (e) { if (current === generation.current) setError(errorMessage(e)); }
    finally { if (current === generation.current) setLoading(false); }
  }, [load, merge]);
  useEffect(() => {
    const counter = generation;
    void refresh();
    return () => { counter.current++; };
  }, [refresh, revision]);
  return { data, setData, error, loading, refresh };
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
