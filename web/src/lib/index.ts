/**
 * `@qern/qpedia-web` — public library surface.
 *
 * The same SvelteKit project that builds the OSS SPA also exposes its
 * reusable bits as a published Svelte package, so `web-pvt` (in the
 * private qpedia-pvt repo) can import components, types, and the API
 * client without forking pages or copying source.
 *
 * Import paths from the overlay:
 *
 *   import { listSources, type Source } from '@qern/qpedia-web';
 *   import FolderTree from '@qern/qpedia-web/components/FolderTree.svelte';
 *   import StatusChip from '@qern/qpedia-web/components/StatusChip.svelte';
 *   import UploadPanel from '@qern/qpedia-web/components/UploadPanel.svelte';
 *   import '@qern/qpedia-web/app.css';   // OSS theme tokens (CSS variables)
 *
 * Overrides the brand by redefining the `--bg`, `--bg-2`, `--accent`,
 * etc. CSS variables in a stylesheet loaded AFTER the OSS one.
 */

// HTTP client + types
export * from './api.js';

// Chat history store (Svelte 5 runes-class; consumed by chat/+page.svelte)
export * from './stores.svelte.js';

// Firebase auth client (provider buttons + session exchange)
export * from './firebase.js';
