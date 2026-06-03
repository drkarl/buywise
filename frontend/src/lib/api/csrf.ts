// CSRF token access for mutating API calls.
//
// The backend's auth middleware (crates/api/src/middleware.rs) rejects every
// POST/PUT/PATCH/DELETE with 403 unless it carries an `X-CSRF-Token` header
// equal to the session's CSRF token. The server sets that token at login in a
// `csrf` cookie that is `Secure; SameSite=Strict` but intentionally NOT
// HttpOnly, so the SPA can read it here and echo it back on writes. The session
// cookie itself is HttpOnly and is sent automatically (same-origin fetch), so we
// only need to surface the CSRF header.

/** Cookie name carrying the CSRF token (mirrors `CSRF_COOKIE` in middleware.rs). */
export const CSRF_COOKIE = 'csrf';
/** Header the backend checks (mirrors `CSRF_HEADER` in middleware.rs). */
export const CSRF_HEADER = 'X-CSRF-Token';

/**
 * Reads the `csrf` cookie from `document.cookie`, URL-decoding the value, or
 * returns `null` when it is absent (unauthenticated, or no `document` under
 * SSR/tests). Callers that get `null` should send no header and let the backend
 * surface the resulting 403 through the normal failure path — we never invent a
 * token or an auth flow here.
 */
export function readCsrfToken(): string | null {
  if (typeof document === 'undefined') return null; // SSR / non-DOM test env
  for (const part of document.cookie.split(';')) {
    const eq = part.indexOf('=');
    if (eq === -1) continue;
    const name = part.slice(0, eq).trim();
    if (name !== CSRF_COOKIE) continue;
    const raw = part.slice(eq + 1).trim();
    try {
      return decodeURIComponent(raw);
    } catch {
      return raw; // value wasn't percent-encoded — use it verbatim
    }
  }
  return null;
}

/**
 * Builds the request headers for a mutating JSON call: always `Content-Type`,
 * plus `X-CSRF-Token` when a token is available. Kept as a helper so every write
 * path stamps the header consistently.
 */
export function jsonMutationHeaders(): Record<string, string> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' };
  const token = readCsrfToken();
  if (token) headers[CSRF_HEADER] = token;
  return headers;
}
