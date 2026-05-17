/* Firebase Auth client. Lazy-initialized so dev-mode builds without
   Firebase env vars don't pull in or fail to construct anything.

   The backend verifies the ID token; the SDK here only does the
   provider OAuth dance and hands us back the token. */

import { initializeApp, type FirebaseApp } from 'firebase/app';
import {
  getAuth,
  signInWithPopup,
  GoogleAuthProvider,
  GithubAuthProvider,
  TwitterAuthProvider,
  OAuthProvider,
  type Auth,
  type UserCredential
} from 'firebase/auth';

export type FirebaseConfig = {
  apiKey: string;
  authDomain: string;
  projectId: string;
};

let cachedApp: FirebaseApp | null = null;
let cachedAuth: Auth | null = null;

export function firebaseConfig(): FirebaseConfig | null {
  const apiKey      = (import.meta as any).env?.VITE_FIREBASE_API_KEY as string | undefined;
  const authDomain  = (import.meta as any).env?.VITE_FIREBASE_AUTH_DOMAIN as string | undefined;
  const projectId   = (import.meta as any).env?.VITE_FIREBASE_PROJECT_ID as string | undefined;
  if (!apiKey || !authDomain || !projectId) return null;
  return { apiKey, authDomain, projectId };
}

export function firebaseAuth(): Auth | null {
  if (cachedAuth) return cachedAuth;
  const cfg = firebaseConfig();
  if (!cfg) return null;
  cachedApp = initializeApp(cfg);
  cachedAuth = getAuth(cachedApp);
  return cachedAuth;
}

/** Provider id strings match Firebase's well-known ids so we can pass
    them straight to OAuthProvider for non-built-in providers. */
export type ProviderId =
  | 'google.com'
  | 'github.com'
  | 'microsoft.com'
  | 'apple.com'
  | 'twitter.com'
  | 'facebook.com'
  | `oidc.${string}`;          // enterprise SSO (configured in Firebase console)

function providerFor(id: ProviderId) {
  switch (id) {
    case 'google.com':   return new GoogleAuthProvider();
    case 'github.com':   return new GithubAuthProvider();
    case 'twitter.com':  return new TwitterAuthProvider();
    default:             return new OAuthProvider(id);
  }
}

export async function signInWith(id: ProviderId): Promise<UserCredential> {
  const auth = firebaseAuth();
  if (!auth) throw new Error('Firebase is not configured (set VITE_FIREBASE_* vars)');
  const provider = providerFor(id);
  return await signInWithPopup(auth, provider);
}

/** Exchange a Firebase ID token for a qpedia session cookie. */
export async function exchangeForSession(idToken: string): Promise<{
  user_id: string;
  tenant: string;
  email: string | null;
  name: string | null;
  provider: string;
  groups: string[];
}> {
  const r = await fetch('/api/v1/auth/firebase/login', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id_token: idToken })
  });
  if (!r.ok) {
    const detail = await r.text().catch(() => '');
    throw new Error(`exchange failed: ${r.status} ${detail.slice(0, 200)}`);
  }
  return r.json();
}
