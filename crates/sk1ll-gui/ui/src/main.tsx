import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import './style.css';

// `?mock=<scenario>` renders against a fake Daemon in a plain browser during development.
const scenario = import.meta.env.DEV ? new URLSearchParams(location.search).get('mock') : null;
if (scenario !== null) (await import('./dev/mock')).installMock(scenario);
// The webview's own menu only offers Reload and Inspect; keep it for text fields, where Cut, Copy and Paste help.
document.addEventListener('contextmenu', event => {
  if (!(event.target instanceof Element && event.target.closest('input, textarea, [contenteditable="true"]'))) event.preventDefault();
});
ReactDOM.createRoot(document.getElementById('root')!).render(<React.StrictMode><App /></React.StrictMode>);
