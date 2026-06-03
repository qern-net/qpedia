<script lang="ts">
  // Static marketing page — no app data, no auth required.
  const repo = 'https://github.com/qern-net/qpedia';

  const pitch = [
    {
      n: '01',
      h: 'The wiki compounds',
      p: 'Every new source can touch ten existing pages — cross-references, syntheses, corrections — maintained in place by the model. Knowledge accumulates instead of being rediscovered at every query.'
    },
    {
      n: '02',
      h: 'Human-inspectable',
      p: 'Each page is a markdown file in a real git repo. Browse it as a normal wiki, diff it, blame it, clone it, push it to GitHub. No vendor lock-in on the artifact.'
    },
    {
      n: '03',
      h: 'Paid once, not per query',
      p: 'Synthesis happens at ingestion, not retrieval. Cost scales with what you add, not how often you ask. Predictable LLM bills.'
    }
  ];

  const pipeline = ['Upload', 'Extract', 'Classify', 'Distill', 'Validate', 'Commit', 'Embed', 'Done'];

  const formats = [
    { k: 'PDF', d: 'two-pass + OCR' },
    { k: 'Office', d: 'docx · pptx · odt · rtf · epub' },
    { k: 'HTML', d: 'readability-distilled' },
    { k: 'Images', d: 'OCR + vision description' },
    { k: 'Audio / Video', d: 'metadata · transcription' },
    { k: 'Archives', d: 'zip auto-expanded' },
    { k: 'Markdown', d: 'and plain text' },
    { k: 'Web pages', d: 'paste a URL' }
  ];

  const features = [
    { t: 'Agentic chat', d: 'Graph-walk retrieval, streamed answers, every claim cited back to its source.' },
    { t: 'Hybrid search', d: 'Vector similarity and BM25 weighted in a single SQL statement.' },
    { t: 'A living graph', d: 'Pages link to pages with [[wikilinks]]; the model keeps the web coherent.' },
    { t: 'Vision & OCR', d: 'Scans are transcribed; photos, charts and diagrams are described in words.' },
    { t: 'Deep taxonomy', d: 'Topics, concepts, entities and comparisons — organized as it grows.' },
    { t: 'Right-to-left', d: 'Arabic, Urdu, Hebrew and more render natively, per block.' },
    { t: 'Live processing', d: 'A real-time queue shows every worker and what it’s grinding through.' },
    { t: 'Lint pass', d: 'Orphans, broken links, index drift, near-duplicates, contradictions.' },
    { t: 'Provenance', d: 'Inline numbered citations; download the original behind any fact.' },
    { t: 'Folder explorer', d: 'Mirror your structure; lock folders the AI must not reorganize.' },
    { t: 'Multi-tenant', d: 'Postgres Row-Level Security isolates every workspace — fails closed.' },
    { t: 'Single sign-on', d: 'Google, Microsoft, GitHub, Apple, X, plus generic OIDC for the enterprise.' }
  ];

  const connectors = [
    { name: 'Google Drive', status: 'live' },
    { name: 'Confluence', status: 'live' },
    { name: 'SharePoint', status: 'soon' },
    { name: 'Slack', status: 'soon' },
    { name: 'GitHub', status: 'soon' }
  ];
</script>

<svelte:head>
  <title>Qpedia — Drop documents in. The model writes the wiki. Ask anything.</title>
  <meta
    name="description"
    content="Qpedia grinds raw documents into a living, linked, citable wiki — then answers anything. Self-hosted, inspectable, Apache-2.0." />
  <meta property="og:title" content="Qpedia — the LLM-maintained knowledge base" />
  <meta property="og:description" content="Drop documents in. The model writes the wiki. Ask anything." />
  <link rel="preconnect" href="https://fonts.googleapis.com" />
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="anonymous" />
  <link
    href="https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,300;9..144,400;9..144,500;9..144,600&family=Space+Grotesk:wght@400;500;600;700&display=swap"
    rel="stylesheet" />
</svelte:head>

<!-- Film-grain texture overlay (pure SVG, no asset) -->
<svg class="grain" aria-hidden="true">
  <filter id="grain-f"><feTurbulence type="fractalNoise" baseFrequency="0.8" numOctaves="2" stitchTiles="stitch" /></filter>
  <rect width="100%" height="100%" filter="url(#grain-f)" />
</svg>

<div class="landing">
  <!-- ───────────────────────── nav ───────────────────────── -->
  <nav class="nav">
    <a class="brand" href="/landing" aria-label="Qpedia home">
      {@render qmark(28)}
      <span class="brand-word">Qpedia</span>
    </a>
    <div class="nav-links">
      <a href="#how">How it works</a>
      <a href="#ingest">Ingests</a>
      <a href="#features">Features</a>
      <a href="#stack">Self-host</a>
    </div>
    <div class="nav-cta">
      <a class="ghost" href={repo} target="_blank" rel="noreferrer noopener">GitHub ↗</a>
      <a class="solid" href="/login">Sign in</a>
    </div>
  </nav>

  <!-- ───────────────────────── hero ───────────────────────── -->
  <div class="hero">
    <div class="hero-copy">
      <p class="eyebrow"><span class="tick"></span> LLM-maintained knowledge base</p>
      <h1 class="headline">
        Drop documents in.<br />
        <em>The model writes</em><br />
        the wiki.<span class="ask"> Ask&nbsp;anything.</span>
      </h1>
      <p class="lede">
        A <span class="hl">quern</span> grinds grain into flour. <strong>Qpedia</strong> grinds raw
        documents into a living, linked, citable wiki — distilled once, queried forever. Self-hosted.
        Inspectable. Yours.
      </p>
      <div class="hero-cta">
        <a class="solid lg" href="/login">Get started →</a>
        <a class="ghost lg" href={repo} target="_blank" rel="noreferrer noopener">Read the source</a>
      </div>
      <p class="trust">Apache-2.0 · Postgres + pgvector · runs on your infrastructure</p>
    </div>

    <!-- animated knowledge graph: sources grind into a linked wiki -->
    <div class="hero-art" aria-hidden="true">
      <div class="orbit">
        <svg viewBox="0 0 420 420" class="graph">
          <defs>
            <radialGradient id="halo" cx="50%" cy="50%" r="50%">
              <stop offset="0%" stop-color="#38bdf8" stop-opacity="0.35" />
              <stop offset="100%" stop-color="#38bdf8" stop-opacity="0" />
            </radialGradient>
          </defs>
          <circle cx="210" cy="210" r="190" fill="url(#halo)" />
          <!-- links -->
          <g class="links" stroke="#38bdf8" stroke-width="1.2" fill="none" opacity="0.5">
            <line x1="210" y1="210" x2="96" y2="120" />
            <line x1="210" y1="210" x2="320" y2="104" />
            <line x1="210" y1="210" x2="338" y2="232" />
            <line x1="210" y1="210" x2="280" y2="330" />
            <line x1="210" y1="210" x2="120" y2="318" />
            <line x1="210" y1="210" x2="78" y2="224" />
            <line x1="96" y1="120" x2="320" y2="104" />
            <line x1="280" y1="330" x2="120" y2="318" />
            <line x1="338" y1="232" x2="320" y2="104" />
            <line x1="78" y1="224" x2="120" y2="318" />
          </g>
          <!-- nodes -->
          <g class="nodes">
            <circle class="node n0" cx="210" cy="210" r="13" />
            <circle class="node" cx="96" cy="120" r="7" />
            <circle class="node" cx="320" cy="104" r="9" />
            <circle class="node" cx="338" cy="232" r="6" />
            <circle class="node" cx="280" cy="330" r="8" />
            <circle class="node" cx="120" cy="318" r="7" />
            <circle class="node" cx="78" cy="224" r="6" />
          </g>
        </svg>
        <!-- source chips feeding the graph -->
        <span class="chip c1">report.pdf</span>
        <span class="chip c2">deck.pptx</span>
        <span class="chip c3">scan.jpg</span>
        <span class="chip c4">notes.md</span>
      </div>
    </div>
  </div>

  <!-- ───────────────────── marquee strip ───────────────────── -->
  <div class="strip" aria-hidden="true">
    <div class="strip-track">
      {#each ['EXTRACT', 'CLASSIFY', 'DISTILL', 'CROSS-LINK', 'CITE', 'EMBED', 'SEARCH', 'ANSWER', 'EXTRACT', 'CLASSIFY', 'DISTILL', 'CROSS-LINK', 'CITE', 'EMBED', 'SEARCH', 'ANSWER'] as word}
        <span>{word}</span><span class="dot">✦</span>
      {/each}
    </div>
  </div>

  <!-- ───────────────────────── pitch ───────────────────────── -->
  <section class="pitch">
    <p class="eyebrow center">// why it’s different</p>
    <div class="pitch-grid">
      {#each pitch as c}
        <article class="pcard">
          <span class="pn">{c.n}</span>
          <h3>{c.h}</h3>
          <p>{c.p}</p>
        </article>
      {/each}
    </div>
  </section>

  <!-- ─────────────────────── how it works ─────────────────── -->
  <section id="how" class="how">
    <div class="sec-head">
      <p class="eyebrow">// the pipeline</p>
      <h2>One pass per document.<br />A wiki that never stops compounding.</h2>
    </div>
    <ol class="pipe">
      {#each pipeline as step, i}
        <li>
          <span class="pi">{String(i + 1).padStart(2, '0')}</span>
          <span class="ps">{step}</span>
          {#if i < pipeline.length - 1}<span class="arr" aria-hidden="true">→</span>{/if}
        </li>
      {/each}
    </ol>
    <p class="how-note">
      Upload → extract (PDF/OCR, HTML, vision, …) → classify → an <strong>agentic loop</strong> writes
      new pages and patches existing ones → validate → git commit → embed into Postgres. Queries hit
      the <em>already-distilled</em> wiki, walk its <code>[[wikilinks]]</code>, and stream an answer
      with citations.
    </p>
  </section>

  <!-- ───────────────────────── ingest ───────────────────────── -->
  <section id="ingest" class="ingest">
    <div class="sec-head">
      <p class="eyebrow">// it eats almost anything</p>
      <h2>Point it at the pile.</h2>
    </div>
    <div class="fmt-grid">
      {#each formats as f}
        <div class="fmt">
          <span class="fmt-k">{f.k}</span>
          <span class="fmt-d">{f.d}</span>
        </div>
      {/each}
    </div>
    <p class="ingest-note">Drag a folder — even a 363-file tree of mixed formats — and watch each branch fill.</p>
  </section>

  <!-- ───────────────────────── features ─────────────────────── -->
  <section id="features" class="features">
    <div class="sec-head">
      <p class="eyebrow">// the full instrument</p>
      <h2>Everything the knowledge base needs.</h2>
    </div>
    <div class="feat-grid">
      {#each features as f}
        <article class="feat">
          <span class="feat-mark" aria-hidden="true"></span>
          <h3>{f.t}</h3>
          <p>{f.d}</p>
        </article>
      {/each}
    </div>
  </section>

  <!-- ───────────────────────── connectors ───────────────────── -->
  <section class="conn">
    <div class="sec-head">
      <p class="eyebrow">// where your documents already live</p>
      <h2>Connect a source. It syncs itself.</h2>
    </div>
    <div class="conn-row">
      {#each connectors as c}
        <span class="conn-chip" class:soon={c.status === 'soon'}>
          {c.name}
          <span class="conn-tag">{c.status === 'live' ? 'live' : 'soon'}</span>
        </span>
      {/each}
    </div>
  </section>

  <!-- ───────────────────────── stack / trust ────────────────── -->
  <section id="stack" class="stack">
    <div class="stack-head">
      <p class="eyebrow">// runs on your metal</p>
      <h2>Two containers. Your data never leaves.</h2>
    </div>
    <div class="stack-grid">
      <div class="sline"><span class="sk">Footprint</span><span class="sv">Rust app + Postgres. Git, OCR, pandoc, pdfium, embeddings and the SPA all inside one image.</span></div>
      <div class="sline"><span class="sk">Database</span><span class="sv">PostgreSQL 17 · pgvector (HNSW) · tsvector. Row-Level Security isolates tenants at the database.</span></div>
      <div class="sline"><span class="sk">LLM</span><span class="sv">Pluggable — Anthropic · OpenAI · OpenRouter · vLLM / Ollama. Air-gapped on-prem supported.</span></div>
      <div class="sline"><span class="sk">Auth</span><span class="sv">Firebase federation: Google · Microsoft · GitHub · Apple · X · OIDC SSO. Backend never holds client secrets.</span></div>
      <div class="sline"><span class="sk">Isolation</span><span class="sv">App connects without <code>BYPASSRLS</code>; a forgotten scope fails closed — every row hidden, loudly.</span></div>
      <div class="sline"><span class="sk">License</span><span class="sv">Apache-2.0. Self-host the whole thing; the wiki is yours to <code>git clone</code> and walk away with.</span></div>
    </div>
  </section>

  <!-- ───────────────────────── final CTA ────────────────────── -->
  <section class="cta">
    <div class="cta-inner">
      {@render qmark(64)}
      <h2>Stand up your own Qpedia.</h2>
      <p>Drop documents in. The model writes the wiki. Ask anything.</p>
      <div class="hero-cta center">
        <a class="solid lg" href="/login">Get started →</a>
        <a class="ghost lg" href={repo} target="_blank" rel="noreferrer noopener">Clone the repo ↗</a>
      </div>
    </div>
  </section>

  <!-- ───────────────────────── footer ───────────────────────── -->
  <footer class="foot">
    <div class="foot-l">
      {@render qmark(22)}
      <span><strong>Qpedia</strong> — an LLM that grinds documents into knowledge.</span>
    </div>
    <div class="foot-r">
      <a href="/login">Sign in</a>
      <a href={repo} target="_blank" rel="noreferrer noopener">GitHub</a>
      <span class="muted">© {new Date().getFullYear()} Qern · qern.net</span>
    </div>
  </footer>
</div>

<!-- The "Q" lettermark, per BRAND.md: ring + 45° tail + four-point spark. -->
{#snippet qmark(size: number)}
  <svg width={size} height={size} viewBox="0 0 64 64" class="qmark" aria-hidden="true">
    <circle cx="29" cy="28" r="17" fill="none" stroke="currentColor" stroke-width="4.5" stroke-linecap="round" />
    <line x1="35" y1="34" x2="45" y2="44" stroke="currentColor" stroke-width="4.5" stroke-linecap="round" />
    <line x1="42.5" y1="41.5" x2="47.5" y2="46.5" stroke="currentColor" stroke-width="4.5" stroke-linecap="round" />
    <line x1="47.5" y1="41.5" x2="42.5" y2="46.5" stroke="currentColor" stroke-width="4.5" stroke-linecap="round" />
  </svg>
{/snippet}

<style>
  /* ============================== tokens / base ============================== */
  :global(html) { scroll-behavior: smooth; }
  .landing {
    --ink: #0f172a;
    --ink-2: #131c30;
    --panel: #1e293b;
    --line: #2b3a52;
    --fg: #e7edf6;
    --dim: #94a3b8;
    --sky: #38bdf8;
    --sky-2: #7dd3fc;
    --sans: 'Space Grotesk', ui-sans-serif, system-ui, sans-serif;
    --serif: 'Fraunces', Georgia, 'Times New Roman', serif;
    --mono: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    position: relative;
    background: radial-gradient(1200px 800px at 78% -8%, #16243f 0%, var(--ink) 46%) no-repeat, var(--ink);
    color: var(--fg);
    font-family: var(--sans);
    line-height: 1.5;
    overflow-x: hidden;
    min-height: 100vh;
  }
  .grain {
    position: fixed; inset: 0; width: 100vw; height: 100vh;
    pointer-events: none; z-index: 9; opacity: 0.04; mix-blend-mode: overlay;
  }
  .landing :global(a) { color: inherit; text-decoration: none; }
  .landing :global(a):hover { text-decoration: none; }
  /* app.css is imported globally by the root layout; neutralise its element
     defaults (e.g. h3 { text-transform: uppercase; color: dim }) that would
     otherwise leak in. :where() keeps specificity 0 so the page's own,
     class-based rules still win. */
  .landing :where(h1, h2, h3) {
    margin: 0; text-transform: none; letter-spacing: normal;
    color: inherit; font-weight: inherit;
  }
  .qmark { color: var(--sky); flex: none; display: block; }
  .eyebrow {
    font-family: var(--mono); font-size: 12px; letter-spacing: 0.18em; text-transform: uppercase;
    color: var(--sky); margin: 0 0 18px; display: flex; align-items: center; gap: 10px;
  }
  .eyebrow.center { justify-content: center; }
  .eyebrow .tick { width: 26px; height: 2px; background: var(--sky); display: inline-block; }

  /* ============================== buttons ============================== */
  .solid, .ghost {
    display: inline-flex; align-items: center; gap: 8px; border-radius: 999px;
    font-weight: 600; font-size: 14px; padding: 10px 18px; transition: all 0.18s ease; white-space: nowrap;
  }
  .solid { background: var(--sky); color: #042234; }
  .solid:hover { background: var(--sky-2); transform: translateY(-1px); }
  .ghost { border: 1px solid var(--line); color: var(--fg); }
  .ghost:hover { border-color: var(--sky); color: var(--sky); }
  .lg { padding: 14px 24px; font-size: 15.5px; }

  /* ============================== nav ============================== */
  .nav {
    position: sticky; top: 0; z-index: 20;
    display: flex; align-items: center; gap: 24px;
    padding: 16px clamp(20px, 5vw, 64px);
    backdrop-filter: blur(12px);
    background: color-mix(in srgb, var(--ink) 72%, transparent);
    border-bottom: 1px solid color-mix(in srgb, var(--line) 60%, transparent);
  }
  .brand { display: flex; align-items: center; gap: 10px; }
  .brand-word { font-weight: 700; font-size: 19px; letter-spacing: 0.01em; }
  .nav-links { display: flex; gap: 26px; margin-left: 14px; }
  .nav-links a { font-size: 14px; color: var(--dim); transition: color 0.15s; }
  .nav-links a:hover { color: var(--fg); }
  .nav-cta { margin-left: auto; display: flex; align-items: center; gap: 12px; }

  /* ============================== hero ============================== */
  .hero {
    display: grid; grid-template-columns: 1.05fr 0.95fr; gap: 40px; align-items: center;
    padding: clamp(48px, 9vw, 120px) clamp(20px, 5vw, 64px) clamp(40px, 6vw, 80px);
    max-width: 1340px; margin: 0 auto;
  }
  .headline {
    font-family: var(--serif); font-weight: 400;
    font-size: clamp(40px, 6.4vw, 88px); line-height: 0.99; letter-spacing: -0.02em;
    margin: 0 0 26px;
  }
  .headline em { font-style: italic; color: var(--sky); font-weight: 300; }
  .headline .ask { font-family: var(--sans); font-weight: 600; font-size: 0.42em; letter-spacing: 0.01em;
    color: var(--dim); display: inline-block; vertical-align: middle; margin-left: 14px; }
  .lede { font-size: clamp(16px, 1.5vw, 19px); color: var(--dim); max-width: 38ch; margin: 0 0 30px; }
  .lede .hl { font-family: var(--serif); font-style: italic; color: var(--fg); }
  .lede strong { color: var(--fg); font-weight: 600; }
  .hero-cta { display: flex; gap: 14px; flex-wrap: wrap; }
  .hero-cta.center { justify-content: center; }
  .trust { font-family: var(--mono); font-size: 12px; color: var(--dim); margin: 26px 0 0; letter-spacing: 0.02em; }

  /* hero art */
  .hero-art { display: flex; justify-content: center; }
  .orbit { position: relative; width: min(440px, 90%); aspect-ratio: 1; }
  .graph { width: 100%; height: 100%; overflow: visible; animation: float 9s ease-in-out infinite; }
  .graph .links line { stroke-dasharray: 4 4; animation: dash 7s linear infinite; }
  .node { fill: var(--sky); }
  .node.n0 { fill: var(--sky-2); filter: drop-shadow(0 0 10px color-mix(in srgb, var(--sky) 70%, transparent)); animation: pulse 3.2s ease-in-out infinite; }
  .nodes circle:not(.n0) { animation: blink 4s ease-in-out infinite; }
  .nodes circle:nth-child(3) { animation-delay: 0.6s; }
  .nodes circle:nth-child(4) { animation-delay: 1.1s; }
  .nodes circle:nth-child(5) { animation-delay: 1.7s; }
  .nodes circle:nth-child(6) { animation-delay: 2.3s; }
  .nodes circle:nth-child(7) { animation-delay: 2.9s; }
  .chip {
    position: absolute; font-family: var(--mono); font-size: 11px; color: var(--fg);
    background: color-mix(in srgb, var(--panel) 80%, transparent); border: 1px solid var(--line);
    padding: 5px 10px; border-radius: 8px; backdrop-filter: blur(4px); white-space: nowrap;
    animation: drift 8s ease-in-out infinite;
  }
  .chip::before { content: ''; position: absolute; left: -3px; top: 50%; width: 6px; height: 6px;
    border-radius: 50%; background: var(--sky); transform: translateY(-50%); }
  .c1 { top: 8%; left: -6%; }
  .c2 { top: 26%; right: -10%; animation-delay: 1.2s; }
  .c3 { bottom: 20%; left: -10%; animation-delay: 2.1s; }
  .c4 { bottom: 4%; right: 2%; animation-delay: 3s; }

  @keyframes float { 50% { transform: translateY(-12px); } }
  @keyframes drift { 50% { transform: translateY(-8px) translateX(3px); } }
  @keyframes pulse { 0%, 100% { r: 13; } 50% { r: 16; } }
  @keyframes blink { 0%, 100% { opacity: 0.55; } 50% { opacity: 1; } }
  @keyframes dash { to { stroke-dashoffset: -160; } }

  /* ============================== marquee ============================== */
  .strip { border-block: 1px solid var(--line); overflow: hidden; background: var(--ink-2); }
  .strip-track {
    display: flex; gap: 26px; align-items: center; white-space: nowrap;
    font-family: var(--mono); font-size: 13px; letter-spacing: 0.22em; color: var(--dim);
    padding: 13px 0; width: max-content; animation: scroll 26s linear infinite;
  }
  .strip-track .dot { color: var(--sky); }
  @keyframes scroll { to { transform: translateX(-50%); } }

  /* ============================== sections shared ============================== */
  section { padding: clamp(56px, 8vw, 110px) clamp(20px, 5vw, 64px); max-width: 1300px; margin: 0 auto; }
  .sec-head { margin-bottom: 48px; }
  .sec-head h2, .stack-head h2, .cta h2 {
    font-family: var(--serif); font-weight: 400; letter-spacing: -0.02em; line-height: 1.05;
    font-size: clamp(28px, 4vw, 50px); margin: 0; max-width: 18ch;
  }
  h3 { font-family: var(--sans); }
  code { font-family: var(--mono); font-size: 0.88em; background: color-mix(in srgb, var(--sky) 14%, transparent);
    color: var(--sky-2); padding: 1px 6px; border-radius: 5px; }

  /* pitch */
  .pitch-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 1px; background: var(--line);
    border: 1px solid var(--line); border-radius: 16px; overflow: hidden; }
  .pcard { background: var(--ink); padding: 40px 32px; transition: background 0.2s; }
  .pcard:hover { background: var(--ink-2); }
  .pn { font-family: var(--mono); font-size: 13px; color: var(--sky); letter-spacing: 0.1em; }
  .pcard h3 { font-size: 24px; margin: 14px 0 12px; font-weight: 600; letter-spacing: -0.01em; }
  .pcard p { color: var(--dim); margin: 0; font-size: 15px; }

  /* how */
  .pipe { list-style: none; display: flex; flex-wrap: wrap; gap: 10px 4px; padding: 0; margin: 0 0 36px; }
  .pipe li { display: flex; align-items: center; gap: 12px; }
  .pipe .pi { font-family: var(--mono); font-size: 11px; color: var(--sky); }
  .pipe .ps {
    font-weight: 600; font-size: 15px; padding: 9px 16px; border: 1px solid var(--line);
    border-radius: 10px; background: var(--ink-2);
  }
  .pipe .arr { color: var(--dim); margin: 0 8px; }
  .how-note, .ingest-note, .how p { color: var(--dim); font-size: 16px; max-width: 62ch; }
  .how-note strong, .how-note em { color: var(--fg); }
  .how-note em { font-family: var(--serif); }

  /* ingest */
  .fmt-grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 14px; margin-bottom: 24px; }
  .fmt {
    display: flex; flex-direction: column; gap: 6px; padding: 22px;
    border: 1px solid var(--line); border-radius: 14px; background: var(--ink-2);
    transition: transform 0.18s, border-color 0.18s;
  }
  .fmt:hover { transform: translateY(-3px); border-color: var(--sky); }
  .fmt-k { font-weight: 700; font-size: 17px; }
  .fmt-d { font-family: var(--mono); font-size: 12px; color: var(--dim); }
  .ingest-note { font-style: italic; }

  /* features */
  .feat-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 18px; }
  .feat { padding: 26px; border: 1px solid var(--line); border-radius: 14px; background: var(--ink); position: relative; }
  .feat:hover { border-color: color-mix(in srgb, var(--sky) 60%, var(--line)); }
  .feat-mark { width: 24px; height: 24px; display: block; margin-bottom: 16px; border-radius: 6px;
    background: color-mix(in srgb, var(--sky) 18%, transparent); position: relative; }
  .feat-mark::after { content: ''; position: absolute; inset: 8px; border-radius: 2px; background: var(--sky); }
  .feat h3 { font-size: 17px; margin: 0 0 8px; font-weight: 600; }
  .feat p { color: var(--dim); font-size: 14px; margin: 0; }

  /* connectors */
  .conn-row { display: flex; flex-wrap: wrap; gap: 14px; }
  .conn-chip {
    display: inline-flex; align-items: center; gap: 12px; padding: 14px 20px;
    border: 1px solid var(--line); border-radius: 999px; font-weight: 600; font-size: 15px; background: var(--ink-2);
  }
  .conn-chip.soon { opacity: 0.6; }
  .conn-tag { font-family: var(--mono); font-size: 10px; text-transform: uppercase; letter-spacing: 0.1em;
    padding: 3px 8px; border-radius: 999px; background: color-mix(in srgb, var(--sky) 18%, transparent); color: var(--sky-2); }
  .conn-chip.soon .conn-tag { background: color-mix(in srgb, var(--dim) 18%, transparent); color: var(--dim); }

  /* stack */
  .stack { display: grid; grid-template-columns: 0.8fr 1.2fr; gap: 48px; align-items: start; }
  .stack-head { position: sticky; top: 90px; }
  .stack-grid { display: flex; flex-direction: column; }
  .sline { display: grid; grid-template-columns: 130px 1fr; gap: 24px; padding: 20px 0; border-top: 1px solid var(--line); }
  .sline:last-child { border-bottom: 1px solid var(--line); }
  .sk { font-family: var(--mono); font-size: 12px; text-transform: uppercase; letter-spacing: 0.12em; color: var(--sky); padding-top: 2px; }
  .sv { color: var(--dim); font-size: 15px; }
  .sv :global(code) { color: var(--sky-2); }

  /* cta */
  .cta { text-align: center; }
  .cta-inner {
    border: 1px solid var(--line); border-radius: 28px; padding: clamp(48px, 7vw, 88px) 24px;
    background:
      radial-gradient(600px 300px at 50% 0%, color-mix(in srgb, var(--sky) 12%, transparent), transparent 70%),
      var(--ink-2);
  }
  .cta-inner .qmark { margin: 0 auto 22px; }
  .cta h2 { margin: 0 auto 12px; }
  .cta p { color: var(--dim); font-size: 17px; margin: 0 0 32px; }

  /* footer */
  .foot {
    display: flex; flex-wrap: wrap; gap: 18px; align-items: center; justify-content: space-between;
    padding: 40px clamp(20px, 5vw, 64px); border-top: 1px solid var(--line);
    max-width: 1300px; margin: 0 auto; font-size: 14px;
  }
  .foot-l { display: flex; align-items: center; gap: 12px; color: var(--dim); }
  .foot-l strong { color: var(--fg); }
  .foot-r { display: flex; align-items: center; gap: 24px; }
  .foot-r a { color: var(--dim); }
  .foot-r a:hover { color: var(--sky); }
  .foot-r .muted { color: color-mix(in srgb, var(--dim) 70%, transparent); font-family: var(--mono); font-size: 12px; }

  /* ============================== responsive ============================== */
  @media (max-width: 940px) {
    .hero { grid-template-columns: 1fr; }
    .hero-art { max-width: 340px; margin: 8px auto 0; }
    .pitch-grid { grid-template-columns: 1fr; }
    .fmt-grid { grid-template-columns: repeat(2, 1fr); }
    .feat-grid { grid-template-columns: 1fr; }
    .stack { grid-template-columns: 1fr; gap: 24px; }
    .stack-head { position: static; }
    .nav-links { display: none; }
  }
  @media (max-width: 520px) {
    .fmt-grid { grid-template-columns: 1fr; }
    .sline { grid-template-columns: 1fr; gap: 6px; }
    .nav-cta .ghost { display: none; }
  }
  @media (prefers-reduced-motion: reduce) {
    .graph, .chip, .node, .nodes circle, .strip-track, .graph .links line { animation: none !important; }
    :global(html) { scroll-behavior: auto; }
  }
</style>
