// Small shared pieces: async loading, toasts, drawer, segmented control, popover pill, problems.
import type { ComponentChildren } from "preact";
import { useCallback, useEffect, useRef, useState } from "preact/hooks";
import { ApiError, type Problem } from "./api";

export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<{ data?: T; error?: unknown; loading: boolean }>({ loading: true });
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    setState(s => ({ ...s, loading: true }));
    fn().then(data => live && setState({ data, loading: false }), error => live && setState(s => ({ data: s.data, error, loading: false })));
    return () => { live = false; };
  }, [...deps, n]);
  return { ...state, reload: useCallback(() => setN(x => x + 1), []) };
}

const toasts = new EventTarget();
export function toast(message: string) { toasts.dispatchEvent(new CustomEvent("toast", { detail: message })); }
export function ToastHost() {
  const [msg, setMsg] = useState<string | null>(null);
  useEffect(() => {
    let t: number | undefined;
    const on = (e: Event) => { setMsg((e as CustomEvent<string>).detail); clearTimeout(t); t = window.setTimeout(() => setMsg(null), 3200); };
    toasts.addEventListener("toast", on);
    return () => toasts.removeEventListener("toast", on);
  }, []);
  return msg ? <div class="toast" role="status">{msg}</div> : null;
}

export function Drawer({ title, onClose, children }: { title: string; onClose: () => void; children: ComponentChildren }) {
  const close = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    close.current?.focus();
    const key = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("keydown", key);
    return () => document.removeEventListener("keydown", key);
  }, [onClose]);
  return (
    <>
      <div class="drawer-bg" onClick={onClose} />
      <div class="drawer" role="dialog" aria-modal="true" aria-label={title}>
        <header><h3>{title}</h3><span class="spacer" /><button ref={close} class="btn" type="button" onClick={onClose}>Close</button></header>
        <div class="content">{children}</div>
      </div>
    </>
  );
}

export function Seg<T extends string | number>({ value, options, onChange, label }: { value: T; options: [T, string][]; onChange: (v: T) => void; label: string }) {
  return (
    <div class="seg" role="group" aria-label={label}>
      {options.map(([v, text]) => <button type="button" key={String(v)} class={v === value ? "on" : ""} aria-pressed={v === value} onClick={() => onChange(v)}>{text}</button>)}
    </div>
  );
}

/** A pill that opens a popover under itself; `children` gets a close function. */
export function Pill({ k, v, mono, ghost, onRemove, children }: { k?: string; v: ComponentChildren; mono?: boolean; ghost?: boolean; onRemove?: () => void; children: (close: () => void) => ComponentChildren }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => { if (!ref.current?.contains(e.target as Node)) setOpen(false); };
    const key = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    document.addEventListener("mousedown", down); document.addEventListener("keydown", key);
    return () => { document.removeEventListener("mousedown", down); document.removeEventListener("keydown", key); };
  }, [open]);
  return (
    <span ref={ref} style={{ position: "relative" }}>
      <button type="button" class={`pill-btn${ghost ? " ghost" : ""}`} aria-haspopup="dialog" aria-expanded={open} onClick={() => setOpen(o => !o)}>
        {k && <span class="k">{k}</span>}
        <span class={`v${mono ? " m" : ""}`}>{v}</span>
        {onRemove ? <span class="x" role="button" aria-label="Remove" onClick={e => { e.stopPropagation(); onRemove(); }}>×</span> : <span class="caret" />}
      </button>
      {open && <div class="pop" role="dialog" style={{ top: "calc(100% + 6px)", left: 0 }}>{children(() => setOpen(false))}</div>}
    </span>
  );
}

export function Problems({ error, problems }: { error?: unknown; problems?: Problem[] }) {
  const list = problems ?? (error instanceof ApiError ? error.problems : []);
  if (list.length) {
    return <div class="problems">{list.map(p => <div class="problem"><span class="code-badge">{p.code}</span><span class="path">{p.path}</span><span>{p.message}</span></div>)}</div>;
  }
  if (error) return <div class="problems"><div class="problem"><span class="code-badge">error</span><span class="path" /><span>{error instanceof Error ? error.message : String(error)}</span></div></div>;
  return null;
}

export function Empty({ children }: { children: ComponentChildren }) {
  return <div class="empty">{children}</div>;
}
