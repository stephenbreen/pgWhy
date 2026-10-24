// CodeMirror 6 SQL editor: PostgreSQL dialect highlighting, keyword + object-name
// completion, SSMS key bindings (F5 / Ctrl+Enter run the selection, or the whole buffer when
// nothing is selected), and error decoration at the server-reported location.

import { useEffect, useRef } from "react";
import { EditorState, Compartment, type Extension } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { sql, PostgreSQL, type SQLNamespace } from "@codemirror/lang-sql";
import { autocompletion, completionKeymap, closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { syntaxHighlighting, HighlightStyle, bracketMatching, indentOnInput } from "@codemirror/language";
import { tags as t } from "@lezer/highlight";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { setDiagnostics, lintGutter, type Diagnostic } from "@codemirror/lint";

export interface RunRequest {
  /** full buffer text */
  text: string;
  /** UTF-16 offsets of what to run; whole buffer when nothing is selected */
  from: number;
  to: number;
  /** true when the user ran an explicit selection */
  selection: boolean;
}

export interface EditorError {
  /** UTF-16 offset into the buffer */
  from: number;
  to: number;
  message: string;
}

export interface SqlEditorHandle {
  view: EditorView;
}

/** Convert a UTF-8 byte offset into a JS (UTF-16) string offset. */
export function byteToCharOffset(text: string, byteOffset: number): number {
  let bytes = 0;
  let i = 0;
  while (i < text.length && bytes < byteOffset) {
    const cp = text.codePointAt(i)!;
    bytes += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    i += cp > 0xffff ? 2 : 1;
  }
  return i;
}

/** Convert a JS (UTF-16) offset into a UTF-8 byte offset. */
export function charToByteOffset(text: string, charOffset: number): number {
  return new TextEncoder().encode(text.slice(0, charOffset)).length;
}

// colours come from CSS variables so light and dark themes both read well
const highlight = HighlightStyle.define([
  { tag: t.keyword, color: "var(--syn-keyword)", fontWeight: "500" },
  { tag: [t.string, t.special(t.string)], color: "var(--syn-string)" },
  { tag: [t.number, t.bool, t.null], color: "var(--syn-number)" },
  { tag: [t.lineComment, t.blockComment], color: "var(--syn-comment)", fontStyle: "italic" },
  { tag: [t.typeName, t.standard(t.name)], color: "var(--syn-type)" },
  { tag: [t.operator, t.punctuation], color: "var(--syn-punct)" },
  { tag: t.special(t.name), color: "var(--syn-special)" },
]);

export const editorTheme = EditorView.theme({
  "&": { height: "100%", fontSize: "13px", backgroundColor: "var(--bg)", color: "var(--fg)" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.5" },
  ".cm-gutters": { backgroundColor: "var(--bg-sunken)", color: "var(--fg-muted)", border: "none" },
  ".cm-activeLine": { backgroundColor: "color-mix(in srgb, var(--accent) 6%, transparent)" },
  ".cm-activeLineGutter": { backgroundColor: "transparent", color: "var(--fg)" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "var(--fg)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "var(--bg-selected) !important" },
  ".cm-tooltip": { backgroundColor: "var(--bg-raised)", border: "1px solid var(--border)" },
  ".cm-diagnostic-error": { borderLeftColor: "var(--danger)" },
  ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy var(--danger)" },
});

export function SqlEditor({
  initialText,
  schema,
  readOnly,
  error,
  onChange,
  onRun,
  onCancel,
  onSave,
  handleRef,
}: {
  initialText: string;
  schema?: SQLNamespace;
  readOnly?: boolean;
  error?: EditorError | null;
  onChange?: (text: string) => void;
  onRun: (req: RunRequest) => void;
  onCancel?: () => void;
  onSave?: () => void;
  handleRef?: { current: SqlEditorHandle | null };
}) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const language = useRef(new Compartment());
  const editable = useRef(new Compartment());
  const callbacks = useRef({ onChange, onRun, onCancel, onSave });
  callbacks.current = { onChange, onRun, onCancel, onSave };

  useEffect(() => {
    const run = (view: EditorView) => {
      const sel = view.state.selection.main;
      const text = view.state.doc.toString();
      const selection = !sel.empty;
      callbacks.current.onRun({ text, from: selection ? sel.from : 0, to: selection ? sel.to : text.length, selection });
      return true;
    };
    const extensions: Extension[] = [
      lineNumbers(),
      highlightActiveLineGutter(),
      history(),
      drawSelection(),
      indentOnInput(),
      bracketMatching(),
      closeBrackets(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      syntaxHighlighting(highlight),
      autocompletion(),
      lintGutter(),
      language.current.of(sql({ dialect: PostgreSQL, upperCaseKeywords: false, schema })),
      editable.current.of(EditorState.readOnly.of(!!readOnly)),
      keymap.of([
        { key: "F5", run, preventDefault: true },
        { key: "Mod-Enter", run, preventDefault: true },
        { key: "Mod-e", run, preventDefault: true },
        { key: "Mod-s", run: () => (callbacks.current.onSave?.(), true), preventDefault: true },
        { key: "Escape", run: () => (callbacks.current.onCancel ? (callbacks.current.onCancel(), true) : false) },
        ...closeBracketsKeymap,
        ...defaultKeymap,
        ...searchKeymap,
        ...historyKeymap,
        ...completionKeymap,
        indentWithTab,
      ]),
      EditorView.updateListener.of((u) => {
        if (u.docChanged) callbacks.current.onChange?.(u.state.doc.toString());
      }),
      editorTheme,
    ];
    const view = new EditorView({ state: EditorState.create({ doc: initialText, extensions }), parent: host.current! });
    viewRef.current = view;
    if (handleRef) handleRef.current = { view };
    return () => {
      view.destroy();
      viewRef.current = null;
      if (handleRef) handleRef.current = null;
    };
    // the editor owns its document after mount; initialText only seeds it
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // completion namespace swaps with the connection/catalog without recreating the editor
  useEffect(() => {
    viewRef.current?.dispatch({
      effects: language.current.reconfigure(sql({ dialect: PostgreSQL, upperCaseKeywords: false, schema })),
    });
  }, [schema]);

  useEffect(() => {
    viewRef.current?.dispatch({ effects: editable.current.reconfigure(EditorState.readOnly.of(!!readOnly)) });
  }, [readOnly]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const len = view.state.doc.length;
    const diagnostics: Diagnostic[] = error
      ? [{ from: Math.min(error.from, len), to: Math.min(Math.max(error.to, error.from + 1), len), severity: "error", message: error.message }]
      : [];
    view.dispatch(setDiagnostics(view.state, diagnostics));
    if (error) view.dispatch({ selection: { anchor: Math.min(error.from, len) }, scrollIntoView: true });
  }, [error]);

  return <div className="sql-editor" ref={host} />;
}
