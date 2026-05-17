<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import {
    firebaseConfig,
    signInWith,
    exchangeForSession,
    type ProviderId
  } from '$lib/firebase';

  let configured = $state(true);
  let busy = $state<ProviderId | null>(null);
  let error = $state<string | null>(null);

  onMount(() => {
    configured = firebaseConfig() !== null;
  });

  type ProviderButton = {
    id: ProviderId;
    label: string;
    style: string;
  };

  const providers: ProviderButton[] = [
    { id: 'google.com',     label: 'Continue with Google',     style: 'background:#fff;color:#222;border:1px solid #ddd;' },
    { id: 'microsoft.com',  label: 'Continue with Microsoft',  style: 'background:#2f2f2f;color:#fff;' },
    { id: 'github.com',     label: 'Continue with GitHub',     style: 'background:#24292f;color:#fff;' },
    { id: 'apple.com',      label: 'Continue with Apple',      style: 'background:#000;color:#fff;' },
    { id: 'twitter.com',    label: 'Continue with X',          style: 'background:#0f1419;color:#fff;' },
    { id: 'facebook.com',   label: 'Continue with Facebook',   style: 'background:#1877f2;color:#fff;' }
  ];

  // Enterprise SSO is a Firebase-side configuration: admin registers an
  // OIDC provider with id e.g. `oidc.acme` in the Firebase console. The
  // button is opt-in via env so we don't show a broken option by default.
  const ssoId = (import.meta as any).env?.VITE_FIREBASE_SSO_PROVIDER_ID as string | undefined;

  async function go(id: ProviderId) {
    busy = id;
    error = null;
    try {
      const cred = await signInWith(id);
      const idToken = await cred.user.getIdToken();
      await exchangeForSession(idToken);
      goto('/', { replaceState: true });
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = null;
    }
  }
</script>

<div class="login-shell">
  <h1>Sign in to Qpedia</h1>

  {#if !configured}
    <div class="card muted">
      <p>Firebase auth isn't configured.</p>
      <p class="mono" style="font-size: 12px;">
        Set <code>VITE_FIREBASE_API_KEY</code>, <code>VITE_FIREBASE_AUTH_DOMAIN</code>,
        and <code>VITE_FIREBASE_PROJECT_ID</code> at build time, plus
        <code>QPEDIA_FIREBASE_PROJECT_ID</code> on the backend.
      </p>
      <p>Running in dev mode? <a href="/">Go to the app</a> — every request is `dev:admin`.</p>
    </div>
  {:else}
    <div class="providers">
      {#each providers as p (p.id)}
        <button
          class="provider"
          style={p.style}
          disabled={busy !== null}
          onclick={() => go(p.id)}
        >
          {busy === p.id ? '…' : p.label}
        </button>
      {/each}
      {#if ssoId}
        <button
          class="provider sso"
          disabled={busy !== null}
          onclick={() => go(ssoId as ProviderId)}
        >
          {busy === ssoId ? '…' : 'Continue with company SSO'}
        </button>
      {/if}
    </div>
    {#if error}
      <div class="card" style="border-color: var(--err); color: var(--err); margin-top: 16px;">{error}</div>
    {/if}
  {/if}
</div>

<style>
  .login-shell {
    max-width: 420px;
    margin: 80px auto 0;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .providers {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .provider {
    padding: 10px 14px;
    border-radius: 6px;
    border: 1px solid transparent;
    font: inherit;
    cursor: pointer;
    text-align: left;
  }
  .provider:disabled { opacity: 0.6; cursor: not-allowed; }
  .provider.sso {
    background: var(--bg-2);
    color: var(--fg);
    border: 1px solid var(--accent);
  }
</style>
