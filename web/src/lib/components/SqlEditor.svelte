<script lang="ts">
	import { onMount } from 'svelte';
	import { EditorState } from '@codemirror/state';
	import { EditorView, keymap, lineNumbers, highlightActiveLine } from '@codemirror/view';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { sql } from '@codemirror/lang-sql';

	// A CodeMirror 6 SQL editor for the database tool. Bindable `value`, Ctrl/⌘+Enter runs (`onrun`).
	// Kept OUT of `prose` so Tailwind typography doesn't restyle CM's own DOM.
	let {
		value = $bindable(''),
		onrun
	}: { value?: string; onrun?: () => void } = $props();

	let host: HTMLDivElement;
	let view: EditorView | null = null;

	onMount(() => {
		view = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: value,
				extensions: [
					lineNumbers(),
					highlightActiveLine(),
					history(),
					sql(),
					keymap.of([
						{
							key: 'Mod-Enter',
							preventDefault: true,
							run: () => {
								onrun?.();
								return true;
							}
						},
						indentWithTab,
						...defaultKeymap,
						...historyKeymap
					]),
					EditorView.updateListener.of((u) => {
						if (u.docChanged) value = u.state.doc.toString();
					}),
					EditorView.theme({
						'&': { fontSize: '12.5px', backgroundColor: '#fff' },
						'.cm-content': { fontFamily: 'ui-monospace, SFMono-Regular, Menlo, monospace' },
						'.cm-gutters': { backgroundColor: '#f8fafc', border: 'none', color: '#94a3b8' },
						'&.cm-focused': { outline: 'none' }
					})
				]
			})
		});
		return () => view?.destroy();
	});

	// Sync external value changes (e.g. loading a sample) into the editor without clobbering edits.
	$effect(() => {
		if (view && value !== view.state.doc.toString()) {
			view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: value } });
		}
	});
</script>

<div
	bind:this={host}
	data-testid="sql-editor"
	class="cm-host max-h-80 min-h-32 overflow-auto rounded-md border border-slate-300 focus-within:border-jed-accent"
></div>
