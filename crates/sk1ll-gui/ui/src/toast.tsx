import type { ReactNode } from 'react';
import { Toast } from '@base-ui/react/toast';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

export type ToastOptions = { title: string; description?: string; tone?: 'default' | 'destructive' };
const manager = Toast.createToastManager();
// Call toast({ title, description?, tone? }) from any screen to announce a result.
export const toast = ({ tone = 'default', ...options }: ToastOptions) => manager.add({ ...options, type: tone });
export function useToast() { return { toast }; }
function ToastList() {
  const { toasts } = Toast.useToastManager();
  return toasts.map(item => <Toast.Root key={item.id} toast={item} className={cn('rounded-lg border bg-popover p-3 text-popover-foreground shadow-lg', item.type === 'destructive' && 'border-destructive text-destructive')}>
    <Toast.Content className="flex items-start gap-3"><div className="flex flex-1 flex-col gap-1"><Toast.Title className="text-sm font-medium" /><Toast.Description className="text-xs" /></div>
      <Toast.Close render={<Button variant="ghost" size="sm" />}>Dismiss</Toast.Close></Toast.Content>
  </Toast.Root>);
}
export function ToastProvider({ children }: { children: ReactNode }) {
  return <Toast.Provider toastManager={manager}>{children}<Toast.Portal><Toast.Viewport className="fixed right-4 bottom-4 flex w-80 max-w-[calc(100vw-2rem)] flex-col gap-2"><ToastList /></Toast.Viewport></Toast.Portal></Toast.Provider>;
}
