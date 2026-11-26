// Port of shadcn-svelte-extras' theme-selector: icon button with a Light / Dark / System menu.
import { useEffect, useState } from 'react';
import { Moon, Sun } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuRadioGroup, DropdownMenuRadioItem, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';

export type Mode = 'light' | 'dark' | 'system';
const key = 'sk1ll-theme';

function stored(): Mode {
  try {
    const value = localStorage.getItem(key);
    return value === 'light' || value === 'dark' ? value : 'system';
  } catch { return 'system'; }
}
export function useMode() {
  const [mode, setMode] = useState<Mode>(stored);
  useEffect(() => {
    const preference = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => {
      const dark = mode === 'dark' || (mode === 'system' && preference.matches);
      document.documentElement.classList.toggle('dark', dark);
      document.documentElement.style.colorScheme = dark ? 'dark' : 'light';
    };
    apply();
    try { if (mode === 'system') localStorage.removeItem(key); else localStorage.setItem(key, mode); } catch { /* not persisted */ }
    preference.addEventListener('change', apply);
    return () => preference.removeEventListener('change', apply);
  }, [mode]);
  return [mode, setMode] as const;
}

export function ThemeSelector({ variant = 'outline' }: { variant?: 'outline' | 'ghost' }) {
  const [mode, setMode] = useMode();
  return <DropdownMenu>
    <DropdownMenuTrigger render={<Button variant={variant} size="icon" className="relative" />}>
      <Sun className="scale-100 rotate-0 transition-all! dark:scale-0 dark:-rotate-90" />
      <Moon className="absolute scale-0 rotate-90 transition-all! dark:scale-100 dark:rotate-0" />
      <span className="sr-only">Toggle theme</span>
    </DropdownMenuTrigger>
    <DropdownMenuContent align="end" className="w-32">
      <DropdownMenuRadioGroup value={mode} onValueChange={value => setMode(value as Mode)}>
        <DropdownMenuRadioItem value="light" closeOnClick>Light</DropdownMenuRadioItem>
        <DropdownMenuRadioItem value="dark" closeOnClick>Dark</DropdownMenuRadioItem>
        <DropdownMenuRadioItem value="system" closeOnClick>System</DropdownMenuRadioItem>
      </DropdownMenuRadioGroup>
    </DropdownMenuContent>
  </DropdownMenu>;
}
