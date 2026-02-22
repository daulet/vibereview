import { nanoid } from 'nanoid';

export interface Env {
  SESSIONS: R2Bucket;
  GITHUB_CLIENT_ID: string;
}

const CORS_ALLOWED_ORIGIN = 'https://vibereview.trustme.workers.dev';
const PUBLIC_INDEX_KEY = 'public/recent.json';
const PUBLIC_INDEX_LIMIT = 1000;
const DEFAULT_PUBLIC_LIST_LIMIT = 50;
const MAX_PUBLIC_LIST_LIMIT = 200;

// Rate limiting: Track uploads per IP (in-memory, resets on worker restart)
const uploadCounts = new Map<string, { count: number; resetAt: number }>();
const RATE_LIMIT = 10; // uploads per hour
const RATE_WINDOW = 60 * 60 * 1000; // 1 hour in ms
const AUTH_CACHE_TTL = 10 * 60 * 1000; // 10 minutes
const authCache = new Map<string, { user: GitHubUser; expiresAt: number }>();

interface GitHubUser {
  id: number;
  login: string;
}

interface UploadRecord {
  id: string;
  fingerprint: string;
  security: 'encrypted' | 'public';
  session_name?: string;
  turn_count?: number;
  uploaded_at: string;
}

interface UserUploadIndex {
  version: 1;
  uploads: UploadRecord[];
}

interface PublicUploadRecord {
  id: string;
  uploaded_at: string;
  session_name?: string;
  turn_count?: number;
  owner_login?: string;
}

interface PublicUploadIndex {
  version: 1;
  uploads: PublicUploadRecord[];
}

function checkRateLimit(ip: string): boolean {
  const now = Date.now();
  const record = uploadCounts.get(ip);

  if (!record || record.resetAt < now) {
    uploadCounts.set(ip, { count: 1, resetAt: now + RATE_WINDOW });
    return true;
  }

  if (record.count >= RATE_LIMIT) {
    return false;
  }

  record.count++;
  return true;
}

function corsHeaders(): HeadersInit {
  return {
    'Access-Control-Allow-Origin': CORS_ALLOWED_ORIGIN,
    'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
    'Access-Control-Allow-Headers':
      'Content-Type, Authorization, X-Session-Fingerprint, X-Session-Name, X-Session-Turn-Count, X-Share-Security',
    Vary: 'Origin',
  };
}

function rejectDisallowedOrigin(request: Request): Response | null {
  const origin = request.headers.get('Origin');
  if (!origin || origin === CORS_ALLOWED_ORIGIN) {
    return null;
  }
  return new Response(JSON.stringify({ error: 'Origin not allowed' }), {
    status: 403,
    headers: {
      'Content-Type': 'application/json',
    },
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const isApiRequest = path.startsWith('/api/');

    if (isApiRequest) {
      const originError = rejectDisallowedOrigin(request);
      if (originError) {
        return originError;
      }
    }

    // Handle CORS preflight
    if (request.method === 'OPTIONS') {
      return new Response(null, {
        status: 204,
        headers: corsHeaders(),
      });
    }

    // API Routes
    if (path === '/api/auth/github/client-id' && request.method === 'GET') {
      return handleGitHubClientId(env);
    }

    if (path === '/api/sessions' && request.method === 'POST') {
      return handleUpload(request, env);
    }

    if (path === '/api/uploads' && request.method === 'GET') {
      return handleListUploads(request, env);
    }

    if (path === '/api/public-uploads' && request.method === 'GET') {
      return handleListPublicUploads(request, env);
    }

    if (path.startsWith('/api/sessions/') && request.method === 'GET') {
      const id = path.slice('/api/sessions/'.length);
      return handleDownload(id, env);
    }

    // Web viewer route
    if (path.startsWith('/s/')) {
      const id = path.slice('/s/'.length);
      return handleViewer(id, request, env);
    }

    // Home page
    if (path === '/') {
      return handleHome();
    }

    return new Response('Not Found', { status: 404 });
  },
};

function securityFromHeader(value: string | null): 'encrypted' | 'public' {
  if (value?.toLowerCase() === 'public') {
    return 'public';
  }
  return 'encrypted';
}

function userIndexKey(userId: number): string {
  return `users/${userId}.json`;
}

async function loadUserIndex(env: Env, userId: number): Promise<UserUploadIndex> {
  const object = await env.SESSIONS.get(userIndexKey(userId));
  if (!object) {
    return { version: 1, uploads: [] };
  }
  try {
    const parsed = JSON.parse(await object.text()) as UserUploadIndex;
    if (parsed.version === 1 && Array.isArray(parsed.uploads)) {
      return parsed;
    }
    return { version: 1, uploads: [] };
  } catch {
    return { version: 1, uploads: [] };
  }
}

async function saveUserIndex(env: Env, userId: number, index: UserUploadIndex): Promise<void> {
  await env.SESSIONS.put(userIndexKey(userId), JSON.stringify(index), {
    httpMetadata: { contentType: 'application/json' },
  });
}

async function loadPublicIndex(env: Env): Promise<PublicUploadIndex> {
  const object = await env.SESSIONS.get(PUBLIC_INDEX_KEY);
  if (!object) {
    return { version: 1, uploads: [] };
  }
  try {
    const parsed = JSON.parse(await object.text()) as PublicUploadIndex;
    if (parsed.version === 1 && Array.isArray(parsed.uploads)) {
      return parsed;
    }
    return { version: 1, uploads: [] };
  } catch {
    return { version: 1, uploads: [] };
  }
}

async function savePublicIndex(env: Env, index: PublicUploadIndex): Promise<void> {
  await env.SESSIONS.put(PUBLIC_INDEX_KEY, JSON.stringify(index), {
    httpMetadata: { contentType: 'application/json' },
  });
}

async function upsertPublicUpload(env: Env, upload: PublicUploadRecord): Promise<void> {
  const index = await loadPublicIndex(env);
  const deduped = index.uploads.filter((item) => item.id !== upload.id);
  deduped.unshift(upload);
  deduped.sort((a, b) => b.uploaded_at.localeCompare(a.uploaded_at));
  index.uploads = deduped.slice(0, PUBLIC_INDEX_LIMIT);
  await savePublicIndex(env, index);
}

async function authenticateGitHubUser(accessToken: string): Promise<GitHubUser | null> {
  const now = Date.now();
  const cached = authCache.get(accessToken);
  if (cached && cached.expiresAt > now) {
    return cached.user;
  }

  const response = await fetch('https://api.github.com/user', {
    headers: {
      Authorization: `Bearer ${accessToken}`,
      Accept: 'application/vnd.github+json',
      'User-Agent': 'vibereview-worker',
    },
  });

  if (!response.ok) {
    return null;
  }

  const body = (await response.json()) as GitHubUser;
  if (!body || typeof body.id !== 'number' || typeof body.login !== 'string') {
    return null;
  }

  authCache.set(accessToken, { user: body, expiresAt: now + AUTH_CACHE_TTL });
  return body;
}

async function requireUser(request: Request, env: Env): Promise<GitHubUser | Response> {
  const auth = request.headers.get('Authorization') || '';
  const prefix = 'Bearer ';
  if (!auth.startsWith(prefix)) {
    return new Response(JSON.stringify({ error: 'Missing bearer token' }), {
      status: 401,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  const token = auth.slice(prefix.length).trim();
  if (!token) {
    return new Response(JSON.stringify({ error: 'Missing bearer token' }), {
      status: 401,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  const user = await authenticateGitHubUser(token);
  if (!user) {
    return new Response(JSON.stringify({ error: 'Invalid GitHub token' }), {
      status: 401,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  return user;
}

function handleGitHubClientId(env: Env): Response {
  const clientId = (env.GITHUB_CLIENT_ID || '').trim();
  if (!clientId) {
    return new Response(JSON.stringify({ error: 'GitHub login is not configured' }), {
      status: 503,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  return new Response(JSON.stringify({ client_id: clientId }), {
    headers: {
      ...corsHeaders(),
      'Content-Type': 'application/json',
    },
  });
}

async function handleListUploads(request: Request, env: Env): Promise<Response> {
  const authResult = await requireUser(request, env);
  if (authResult instanceof Response) {
    return authResult;
  }

  const index = await loadUserIndex(env, authResult.id);
  const baseUrl = new URL(request.url).origin;
  const uploads = index.uploads.map((item) => ({
    ...item,
    url: `${baseUrl}/s/${item.id}`,
  }));

  return new Response(JSON.stringify({ uploads }), {
    headers: {
      ...corsHeaders(),
      'Content-Type': 'application/json',
    },
  });
}

async function handleListPublicUploads(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const limitRaw = Number.parseInt(url.searchParams.get('limit') || '', 10);
  const limit =
    Number.isFinite(limitRaw) && limitRaw > 0
      ? Math.min(limitRaw, MAX_PUBLIC_LIST_LIMIT)
      : DEFAULT_PUBLIC_LIST_LIMIT;

  const index = await loadPublicIndex(env);
  const baseUrl = url.origin;
  const uploads = index.uploads
    .slice()
    .sort((a, b) => b.uploaded_at.localeCompare(a.uploaded_at))
    .slice(0, limit)
    .map((item) => ({
    ...item,
    security: 'public' as const,
    url: `${baseUrl}/s/${item.id}`,
    }));

  return new Response(JSON.stringify({ uploads }), {
    headers: {
      ...corsHeaders(),
      'Content-Type': 'application/json',
      'Cache-Control': 'public, max-age=30',
    },
  });
}

async function handleUpload(request: Request, env: Env): Promise<Response> {
  const authResult = await requireUser(request, env);
  if (authResult instanceof Response) {
    return authResult;
  }

  const fingerprint = request.headers.get('X-Session-Fingerprint')?.trim();
  if (!fingerprint) {
    return new Response(JSON.stringify({ error: 'Missing X-Session-Fingerprint header' }), {
      status: 400,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  const security = securityFromHeader(request.headers.get('X-Share-Security'));
  const sessionNameRaw = request.headers.get('X-Session-Name')?.trim();
  const sessionName = sessionNameRaw ? sessionNameRaw.slice(0, 180) : undefined;
  const turnCountRaw = request.headers.get('X-Session-Turn-Count');
  const turnCount = turnCountRaw ? Number.parseInt(turnCountRaw, 10) : undefined;
  const normalizedTurnCount =
    typeof turnCount === 'number' && Number.isFinite(turnCount) && turnCount >= 0
      ? turnCount
      : undefined;

  const index = await loadUserIndex(env, authResult.id);
  const existing = index.uploads.find(
    (item) => item.fingerprint === fingerprint && item.security === security
  );
  if (existing) {
    if (security === 'public') {
      await upsertPublicUpload(env, {
        id: existing.id,
        uploaded_at: existing.uploaded_at,
        session_name: existing.session_name,
        turn_count: existing.turn_count,
        owner_login: authResult.login,
      });
    }
    const baseUrl = new URL(request.url).origin;
    return new Response(
      JSON.stringify({
        id: existing.id,
        url: `${baseUrl}/s/${existing.id}`,
        reused: true,
      }),
      {
        status: 200,
        headers: {
          ...corsHeaders(),
          'Content-Type': 'application/json',
        },
      }
    );
  }

  // Check rate limit
  const ip = request.headers.get('CF-Connecting-IP') || 'unknown';
  if (!checkRateLimit(ip)) {
    return new Response(JSON.stringify({ error: 'Rate limit exceeded. Try again later.' }), {
      status: 429,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  // Get compressed body
  const body = await request.arrayBuffer();
  if (body.byteLength === 0) {
    return new Response(JSON.stringify({ error: 'Empty body' }), {
      status: 400,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  // Limit size to 10MB
  if (body.byteLength > 10 * 1024 * 1024) {
    return new Response(JSON.stringify({ error: 'Session too large (max 10MB)' }), {
      status: 413,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  // Generate ID (12 chars, URL-safe)
  const id = nanoid(12);
  const uploadedAt = new Date().toISOString();

  // Store in R2
  await env.SESSIONS.put(id, body, {
    httpMetadata: {
      contentType: 'application/octet-stream',
    },
    customMetadata: {
      uploadedAt,
      ip: ip,
      ownerId: String(authResult.id),
      ownerLogin: authResult.login,
      fingerprint,
      security,
      turnCount: normalizedTurnCount?.toString() ?? '',
      sessionName: sessionName ?? '',
    },
  });

  const newRecord: UploadRecord = {
    id,
    fingerprint,
    security,
    session_name: sessionName,
    turn_count: normalizedTurnCount,
    uploaded_at: uploadedAt,
  };
  index.uploads.unshift(newRecord);
  index.uploads = index.uploads.slice(0, 1000);
  await saveUserIndex(env, authResult.id, index);

  if (security === 'public') {
    await upsertPublicUpload(env, {
      id,
      uploaded_at: uploadedAt,
      session_name: sessionName,
      turn_count: normalizedTurnCount,
      owner_login: authResult.login,
    });
  }

  const baseUrl = new URL(request.url).origin;
  const shareUrl = `${baseUrl}/s/${id}`;

  return new Response(
    JSON.stringify({
      id,
      url: shareUrl,
      reused: false,
    }),
    {
      status: 201,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    }
  );
}

async function handleDownload(id: string, env: Env): Promise<Response> {
  // Validate ID format (should be nanoid 12 chars)
  if (!/^[A-Za-z0-9_-]{12}$/.test(id)) {
    return new Response(JSON.stringify({ error: 'Invalid session ID' }), {
      status: 400,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  const object = await env.SESSIONS.get(id);
  if (!object) {
    return new Response(JSON.stringify({ error: 'Session not found' }), {
      status: 404,
      headers: {
        ...corsHeaders(),
        'Content-Type': 'application/json',
      },
    });
  }

  return new Response(object.body, {
    headers: {
      ...corsHeaders(),
      'Content-Type': 'application/octet-stream',
      'Cache-Control': 'public, max-age=31536000, immutable',
      'X-Robots-Tag': 'noindex',
    },
  });
}

function handleHome(): Response {
  const html = generateHomeHtml();
  return new Response(html, {
    headers: {
      'Content-Type': 'text/html; charset=utf-8',
      'Cache-Control': 'public, max-age=60',
    },
  });
}

async function handleViewer(id: string, request: Request, env: Env): Promise<Response> {
  // Validate ID format
  if (!/^[A-Za-z0-9_-]{12}$/.test(id)) {
    return new Response('Invalid session ID', { status: 400 });
  }

  // Check if session exists
  const object = await env.SESSIONS.head(id);
  if (!object) {
    return new Response('Session not found', { status: 404 });
  }

  // Return the web viewer HTML
  const html = generateViewerHtml(id);

  return new Response(html, {
    headers: {
      'Content-Type': 'text/html; charset=utf-8',
      'X-Robots-Tag': 'noindex',
      'Cache-Control': 'no-cache',
    },
  });
}

function generateHomeHtml(): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>VibeReview - Public Sessions</title>
  <style>
    @import url('https://fonts.googleapis.com/css2?family=Sora:wght@400;600;700&family=IBM+Plex+Mono:wght@400;500&display=swap');

    :root {
      --bg: #f4f7f2;
      --bg-elev: #ffffff;
      --ink: #15231f;
      --muted: #49605a;
      --line: #d6e2db;
      --accent: #067a62;
      --accent-soft: #daf7ef;
      --shadow: 0 14px 30px rgba(12, 52, 41, 0.1);
    }

    * {
      box-sizing: border-box;
    }

    html,
    body {
      margin: 0;
      padding: 0;
      min-height: 100%;
      background:
        radial-gradient(1200px 600px at 10% -10%, #d5eee5 0%, transparent 70%),
        radial-gradient(900px 500px at 100% 0%, #e7f2dc 0%, transparent 65%),
        var(--bg);
      color: var(--ink);
      font-family: 'Sora', 'Avenir Next', 'Segoe UI', sans-serif;
    }

    .page {
      max-width: 980px;
      margin: 0 auto;
      padding: 28px 18px 40px;
    }

    .hero {
      background: linear-gradient(130deg, #ffffff 0%, #f3fbf7 60%, #f7f9ec 100%);
      border: 1px solid var(--line);
      border-radius: 18px;
      padding: 22px;
      box-shadow: var(--shadow);
      animation: rise 220ms ease-out;
    }

    .eyebrow {
      display: inline-block;
      padding: 6px 10px;
      border-radius: 999px;
      background: var(--accent-soft);
      color: #035341;
      font: 500 12px/1 'IBM Plex Mono', monospace;
      letter-spacing: 0.03em;
      text-transform: uppercase;
    }

    h1 {
      margin: 10px 0 10px;
      font-size: clamp(1.8rem, 4.6vw, 2.6rem);
      line-height: 1.06;
      letter-spacing: -0.02em;
    }

    .hero p {
      margin: 0;
      color: var(--muted);
      max-width: 70ch;
      line-height: 1.55;
    }

    .meta {
      margin-top: 14px;
      color: var(--muted);
      font: 500 12px/1 'IBM Plex Mono', monospace;
    }

    .list-head {
      margin: 24px 4px 12px;
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 12px;
    }

    .list-head h2 {
      margin: 0;
      font-size: 1.05rem;
      font-weight: 700;
      letter-spacing: 0.01em;
    }

    .list-head .hint {
      color: var(--muted);
      font: 500 12px/1 'IBM Plex Mono', monospace;
    }

    .uploads {
      display: grid;
      gap: 10px;
    }

    .upload-card {
      text-decoration: none;
      color: inherit;
      border: 1px solid var(--line);
      border-radius: 14px;
      background: var(--bg-elev);
      box-shadow: 0 7px 18px rgba(15, 56, 44, 0.07);
      padding: 14px 16px;
      display: block;
      transition: transform 140ms ease, box-shadow 140ms ease, border-color 140ms ease;
      animation: rise 220ms ease-out both;
    }

    .upload-card:hover {
      transform: translateY(-1px);
      border-color: #9bc8bc;
      box-shadow: 0 10px 22px rgba(13, 52, 42, 0.12);
    }

    .upload-title {
      margin: 0 0 7px;
      font-size: 1rem;
      font-weight: 600;
      line-height: 1.35;
      word-break: break-word;
    }

    .upload-meta {
      margin: 0;
      color: var(--muted);
      font: 500 12px/1.4 'IBM Plex Mono', monospace;
    }

    .empty {
      border: 1px dashed #b8cbc3;
      border-radius: 14px;
      background: #fbfdfb;
      padding: 16px;
      color: var(--muted);
      line-height: 1.5;
    }

    .status {
      color: var(--muted);
      font: 500 13px/1.5 'IBM Plex Mono', monospace;
      margin: 0 4px;
    }

    @keyframes rise {
      from {
        opacity: 0;
        transform: translateY(8px);
      }
      to {
        opacity: 1;
        transform: translateY(0);
      }
    }

    @media (max-width: 680px) {
      .page {
        padding: 18px 12px 28px;
      }
      .hero {
        padding: 18px 14px;
      }
      .upload-card {
        padding: 12px 12px;
      }
      .list-head {
        flex-direction: column;
        align-items: flex-start;
      }
    }
  </style>
</head>
<body>
  <main class="page">
    <section class="hero">
      <span class="eyebrow">VibeReview Cloud</span>
      <h1>Recent Public Sessions</h1>
      <p>
        Browse sessions that were shared in public mode. Click any item to open it in the viewer.
        Encrypted shares stay private and do not appear on this page.
      </p>
      <div class="meta">Source: /api/public-uploads</div>
    </section>

    <div class="list-head">
      <h2>Latest uploads</h2>
      <span class="hint">Most recent first</span>
    </div>
    <p id="status" class="status">Loading public uploads...</p>
    <section id="uploads" class="uploads" aria-live="polite"></section>
  </main>

  <script>
    const statusEl = document.getElementById('status');
    const uploadsEl = document.getElementById('uploads');

    function formatDate(isoValue) {
      const date = new Date(isoValue);
      if (Number.isNaN(date.valueOf())) {
        return isoValue || 'unknown';
      }
      return new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium',
        timeStyle: 'short',
      }).format(date);
    }

    function makeMeta(item) {
      const parts = [];
      if (typeof item.turn_count === 'number') {
        parts.push(item.turn_count + ' turns');
      }
      if (item.owner_login) {
        parts.push('by @' + item.owner_login);
      }
      parts.push(formatDate(item.uploaded_at));
      return parts.join(' · ');
    }

    function renderEmpty() {
      uploadsEl.innerHTML = '';
      const div = document.createElement('div');
      div.className = 'empty';
      div.textContent = 'No public uploads yet. Share a session in public mode to populate this page.';
      uploadsEl.appendChild(div);
    }

    function renderUploads(uploads) {
      uploadsEl.innerHTML = '';
      if (!Array.isArray(uploads) || uploads.length === 0) {
        renderEmpty();
        return;
      }

      uploads.forEach((item, index) => {
        const card = document.createElement('a');
        card.className = 'upload-card';
        card.href = item.url;
        card.style.animationDelay = (index * 30) + 'ms';

        const title = document.createElement('h3');
        title.className = 'upload-title';
        title.textContent = item.session_name || 'Untitled session';

        const meta = document.createElement('p');
        meta.className = 'upload-meta';
        meta.textContent = makeMeta(item);

        card.appendChild(title);
        card.appendChild(meta);
        uploadsEl.appendChild(card);
      });
    }

    async function loadPublicUploads() {
      try {
        const response = await fetch('/api/public-uploads?limit=80', {
          headers: { Accept: 'application/json' }
        });
        if (!response.ok) {
          throw new Error('HTTP ' + response.status);
        }
        const payload = await response.json();
        const uploads = Array.isArray(payload.uploads) ? payload.uploads : [];
        statusEl.textContent = uploads.length + ' public ' + (uploads.length === 1 ? 'upload' : 'uploads');
        renderUploads(uploads);
      } catch (error) {
        statusEl.textContent = 'Failed to load public uploads.';
        uploadsEl.innerHTML = '';
        const div = document.createElement('div');
        div.className = 'empty';
        div.textContent = 'Try refreshing in a moment.';
        uploadsEl.appendChild(div);
      }
    }

    loadPublicUploads();
  </script>
</body>
</html>`;
}

function generateViewerHtml(sessionId: string): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta name="robots" content="noindex, nofollow">
  <title>VibeReview - Shared Session</title>
  <style>
    :root {
      --bg-primary: #1a1a2e;
      --bg-secondary: #16213e;
      --bg-tertiary: #0f3460;
      --text-primary: #e8e8e8;
      --text-secondary: #a0a0a0;
      --text-muted: #666;
      --accent-cyan: #00d9ff;
      --accent-green: #00ff88;
      --accent-yellow: #ffd700;
      --accent-magenta: #ff00ff;
      --accent-red: #ff4444;
      --border-color: #333;
    }

    * {
      box-sizing: border-box;
      margin: 0;
      padding: 0;
    }

    body {
      font-family: 'Monaco', 'Menlo', 'Ubuntu Mono', monospace;
      font-size: 13px;
      line-height: 1.5;
      background: var(--bg-primary);
      color: var(--text-primary);
      height: 100vh;
      overflow: hidden;
    }

    .container {
      display: flex;
      height: 100vh;
      width: 100vw;
      overflow: hidden;
    }

    .turn-list {
      width: 30%;
      min-width: 250px;
      max-width: 400px;
      flex-shrink: 0;
      border-right: 1px solid var(--border-color);
      display: flex;
      flex-direction: column;
      overflow: hidden;
    }

    .turn-list-header {
      padding: 12px;
      flex-shrink: 0;
      border-bottom: 1px solid var(--border-color);
      background: var(--bg-secondary);
    }

    .turn-list-title {
      color: var(--accent-cyan);
      font-weight: bold;
    }

    .turn-list-items {
      flex: 1;
      min-height: 0;
      overflow-y: auto;
    }

    .turn-item {
      padding: 8px 12px;
      cursor: pointer;
      border-bottom: 1px solid var(--border-color);
      display: flex;
      align-items: center;
      gap: 8px;
    }

    .turn-item:hover {
      background: var(--bg-secondary);
    }

    .turn-item.selected {
      background: var(--bg-tertiary);
    }

    .turn-item.selected::before {
      content: '\\25B6';
      color: var(--accent-yellow);
    }

    .turn-number {
      color: var(--text-muted);
      min-width: 24px;
    }

    .turn-preview {
      flex: 1;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .turn-tools {
      color: var(--accent-cyan);
      font-size: 11px;
    }

    .detail-panel {
      flex: 1;
      min-width: 0;
      display: flex;
      flex-direction: column;
      overflow: hidden;
    }

    .breadcrumb {
      padding: 8px 12px;
      flex-shrink: 0;
      background: var(--bg-secondary);
      color: var(--accent-cyan);
      font-size: 12px;
      display: none;
    }

    .breadcrumb.visible {
      display: block;
    }

    .tabs {
      display: flex;
      flex-shrink: 0;
      border-bottom: 1px solid var(--border-color);
      background: var(--bg-secondary);
    }

    .tab {
      padding: 10px 20px;
      cursor: pointer;
      border-bottom: 2px solid transparent;
      color: var(--text-secondary);
      transition: all 0.2s;
    }

    .tab:hover {
      color: var(--text-primary);
      background: var(--bg-tertiary);
    }

    .tab.active {
      color: var(--accent-yellow);
      border-bottom-color: var(--accent-yellow);
      font-weight: bold;
    }

    .content {
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      overflow-x: hidden;
      padding: 16px;
    }

    .section-header {
      color: var(--accent-cyan);
      font-weight: bold;
      margin-bottom: 8px;
    }

    .section-header.green {
      color: var(--accent-green);
    }

    .section-header.magenta {
      color: var(--accent-magenta);
    }

    .divider {
      color: var(--text-muted);
      margin: 16px 0;
    }

    .response-header {
      color: var(--accent-green);
      font-weight: bold;
      margin-top: 16px;
      margin-bottom: 8px;
    }

    pre {
      white-space: pre-wrap;
      overflow-wrap: break-word;
      max-width: 100%;
      word-break: break-word;
    }

    .tool-list {
      display: flex;
      flex-direction: column;
      gap: 4px;
    }

    .tool-item {
      padding: 8px;
      border-radius: 4px;
      cursor: pointer;
      display: flex;
      align-items: center;
      gap: 8px;
    }

    .tool-item:hover {
      background: var(--bg-secondary);
    }

    .tool-item.selected {
      background: var(--bg-tertiary);
    }

    .tool-item.selected::before {
      content: '\\25CF';
      color: var(--accent-yellow);
    }

    .tool-item.openable.selected::before {
      content: '\\25B6';
    }

    .tool-number {
      color: var(--text-muted);
      font-size: 11px;
    }

    .tool-name {
      font-weight: bold;
    }

    .tool-name.selected {
      color: var(--accent-yellow);
    }

    .tool-name.openable {
      color: var(--accent-magenta);
    }

    .tool-context {
      color: var(--text-muted);
      flex: 1;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .tool-detail {
      margin-top: 16px;
      padding: 12px;
      background: var(--bg-secondary);
      border-radius: 4px;
    }

    .tool-detail-header {
      font-weight: bold;
      margin-bottom: 8px;
    }

    .tool-detail-header.input {
      color: var(--accent-green);
    }

    .tool-detail-header.output {
      color: var(--accent-yellow);
    }

    .tool-detail-content {
      padding-left: 16px;
      max-height: 300px;
      overflow-y: auto;
    }

    .diff-line {
      font-family: monospace;
    }

    .diff-add {
      color: var(--accent-green);
    }

    .diff-remove {
      color: var(--accent-red);
    }

    .diff-header {
      color: var(--accent-cyan);
      font-weight: bold;
    }

    .diff-hunk {
      color: var(--accent-cyan);
    }

    .empty-state {
      color: var(--text-muted);
      padding: 20px;
      text-align: center;
    }

    .help-bar {
      padding: 8px 12px;
      flex-shrink: 0;
      background: var(--bg-secondary);
      border-top: 1px solid var(--border-color);
      color: var(--text-muted);
      font-size: 11px;
    }

    .loading {
      display: flex;
      align-items: center;
      justify-content: center;
      height: 100vh;
      color: var(--accent-cyan);
    }

    .error {
      display: flex;
      align-items: center;
      justify-content: center;
      height: 100vh;
      color: var(--accent-red);
    }

    /* Subagent hint */
    .subagent-hint {
      color: var(--accent-magenta);
      font-style: italic;
      margin-top: 12px;
    }

    /* Markdown styles */
    .markdown-content h1,
    .markdown-content h2,
    .markdown-content h3,
    .markdown-content h4 {
      color: var(--accent-cyan);
      margin-top: 16px;
      margin-bottom: 8px;
    }

    .markdown-content h1 { font-size: 1.4em; }
    .markdown-content h2 { font-size: 1.2em; }
    .markdown-content h3 { font-size: 1.1em; }

    .markdown-content p {
      margin-bottom: 12px;
    }

    .markdown-content code {
      background: #2a2a3e;
      color: var(--accent-green);
      padding: 2px 6px;
      border-radius: 3px;
      font-family: inherit;
    }

    .markdown-content pre {
      background: #2a2a3e;
      padding: 12px;
      border-radius: 4px;
      overflow-x: auto;
      margin: 12px 0;
    }

    .markdown-content pre code {
      background: transparent;
      padding: 0;
      color: var(--text-primary);
    }

    .markdown-content ul,
    .markdown-content ol {
      margin-left: 20px;
      margin-bottom: 12px;
    }

    .markdown-content li {
      margin-bottom: 4px;
    }

    .markdown-content blockquote {
      border-left: 3px solid var(--accent-cyan);
      margin: 12px 0;
      padding-left: 12px;
      color: var(--text-secondary);
    }

    .markdown-content a {
      color: var(--accent-cyan);
      text-decoration: none;
    }

    .markdown-content a:hover {
      text-decoration: underline;
    }

    .markdown-content strong {
      color: var(--accent-yellow);
    }

    .markdown-content em {
      color: var(--accent-magenta);
    }

    .markdown-content hr {
      border: none;
      border-top: 1px solid var(--border-color);
      margin: 16px 0;
    }

    .markdown-content table {
      border-collapse: collapse;
      margin: 12px 0;
      width: 100%;
    }

    .markdown-content th,
    .markdown-content td {
      border: 1px solid var(--border-color);
      padding: 8px;
      text-align: left;
    }

    .markdown-content th {
      background: var(--bg-secondary);
      color: var(--accent-cyan);
    }
  </style>
</head>
<body>
  <div id="app" class="loading">Loading session...</div>

  <script type="module">
    import { decompress } from 'https://esm.sh/fzstd@0.1.1';
    import { marked } from 'https://esm.sh/marked@15.0.0';

    // Configure marked for safe output (no raw HTML pass-through)
    marked.setOptions({
      gfm: true,
      breaks: true,
    });

    const SESSION_ID = '${sessionId}';
    const API_URL = '/api/sessions/' + SESSION_ID;
    const CLOUD_SHARE_MAGIC = [0x56, 0x52, 0x45, 0x31]; // "VRE1"
    const NONCE_LEN = 12;

    let session = null;
    let turns = [];
    let contextStack = [];
    let selectedTurnIndex = 0;
    let selectedToolIndex = 0;
    let activeTab = 'prompt';

    function getShareKeyFromLocation() {
      const queryParams = new URLSearchParams(window.location.search);
      const queryKey = queryParams.get('k') || queryParams.get('key');
      if (queryKey) {
        return queryKey;
      }

      const fragment = window.location.hash.startsWith('#')
        ? window.location.hash.slice(1)
        : window.location.hash;
      if (!fragment) {
        return null;
      }

      if (!fragment.includes('=')) {
        return fragment;
      }

      const fragmentParams = new URLSearchParams(fragment);
      return fragmentParams.get('k') || fragmentParams.get('key');
    }

    function base64UrlToBytes(value) {
      let base64 = value.replace(/-/g, '+').replace(/_/g, '/');
      while (base64.length % 4 !== 0) {
        base64 += '=';
      }
      const raw = atob(base64);
      const bytes = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i++) {
        bytes[i] = raw.charCodeAt(i);
      }
      return bytes;
    }

    function isEncryptedPayload(payload) {
      if (payload.length < CLOUD_SHARE_MAGIC.length + NONCE_LEN + 16) {
        return false;
      }
      return CLOUD_SHARE_MAGIC.every((byte, i) => payload[i] === byte);
    }

    async function decryptPayload(payload, keyBytes) {
      const nonceStart = CLOUD_SHARE_MAGIC.length;
      const nonceEnd = nonceStart + NONCE_LEN;
      const nonce = payload.slice(nonceStart, nonceEnd);
      const ciphertext = payload.slice(nonceEnd);

      const key = await crypto.subtle.importKey(
        'raw',
        keyBytes,
        { name: 'AES-GCM' },
        false,
        ['decrypt']
      );

      const decrypted = await crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: nonce },
        key,
        ciphertext
      );
      return new Uint8Array(decrypted);
    }

    async function loadSession() {
      try {
        const response = await fetch(API_URL);
        if (!response.ok) {
          throw new Error('Session not found');
        }

        const payload = new Uint8Array(await response.arrayBuffer());
        let compressed = payload;

        if (isEncryptedPayload(payload)) {
          const encodedKey = getShareKeyFromLocation();
          if (!encodedKey) {
            throw new Error("Missing share key. Add '#k=<key>' to the URL.");
          }

          const keyBytes = base64UrlToBytes(encodedKey);
          if (keyBytes.length !== 32) {
            throw new Error('Invalid share key');
          }

          try {
            compressed = await decryptPayload(payload, keyBytes);
          } catch {
            throw new Error('Failed to decrypt payload. Check the share key in the URL.');
          }
        }

        const decompressed = decompress(compressed);
        const text = new TextDecoder().decode(decompressed);
        const data = JSON.parse(text);

        session = data.session;
        turns = session.turns;
        contextStack = [{ title: session.name, turns }];

        render();
      } catch (error) {
        document.getElementById('app').innerHTML =
          '<div class="error">Error loading session: ' + error.message + '</div>';
      }
    }

    function getCurrentContext() {
      return contextStack[contextStack.length - 1];
    }

    function render() {
      const ctx = getCurrentContext();
      if (!ctx) return;

      const turn = ctx.turns[selectedTurnIndex];

      document.getElementById('app').innerHTML = \`
        <div class="container">
          <div class="turn-list">
            <div class="turn-list-header">
              <span class="turn-list-title">Turns (\${ctx.turns.length})</span>
            </div>
            <div class="turn-list-items">
              \${ctx.turns.map((t, i) => \`
                <div class="turn-item \${i === selectedTurnIndex ? 'selected' : ''}" data-index="\${i}">
                  <span class="turn-number">\${i + 1}:</span>
                  <span class="turn-preview">\${escapeHtml(truncate(t.user_prompt, 40))}</span>
                  \${t.tool_invocations?.length ? \`<span class="turn-tools">[\${t.tool_invocations.length}]</span>\` : ''}
                </div>
              \`).join('')}
            </div>
          </div>
          <div class="detail-panel">
            <div class="breadcrumb \${contextStack.length > 1 ? 'visible' : ''}">
              \${contextStack.map(c => c.title).join(' > ')}
            </div>
            <div class="tabs">
              <div class="tab \${activeTab === 'prompt' ? 'active' : ''}" data-tab="prompt">Prompt</div>
              <div class="tab \${activeTab === 'thinking' ? 'active' : ''}" data-tab="thinking">Thinking</div>
              <div class="tab \${activeTab === 'tools' ? 'active' : ''}" data-tab="tools">Tool Calls</div>
              <div class="tab \${activeTab === 'diff' ? 'active' : ''}" data-tab="diff">Diff</div>
            </div>
            <div class="content">
              \${renderTabContent(turn)}
            </div>
            <div class="help-bar">
              \\u2191/\\u2193: Navigate | \\u2190/\\u2192: Tabs | j/k: Scroll/Tools | Enter: Open subagent | Esc: Back
            </div>
          </div>
        </div>
      \`;

      // Add event listeners
      document.querySelectorAll('.turn-item').forEach(el => {
        el.addEventListener('click', () => {
          selectedTurnIndex = parseInt(el.dataset.index);
          selectedToolIndex = 0;
          render();
        });
      });

      document.querySelectorAll('.tab').forEach(el => {
        el.addEventListener('click', () => {
          activeTab = el.dataset.tab;
          render();
        });
      });

      document.querySelectorAll('.tool-item').forEach(el => {
        el.addEventListener('click', () => {
          selectedToolIndex = parseInt(el.dataset.index);
          render();
        });
        el.addEventListener('dblclick', () => {
          tryOpenSubagent();
        });
      });
    }

    function renderTabContent(turn) {
      if (!turn) return '<div class="empty-state">Select a turn to view details</div>';

      switch (activeTab) {
        case 'prompt':
          return renderPromptTab(turn);
        case 'thinking':
          return renderThinkingTab(turn);
        case 'tools':
          return renderToolsTab(turn);
        case 'diff':
          return renderDiffTab(turn);
        default:
          return '';
      }
    }

    function renderPromptTab(turn) {
      return \`
        <div class="section-header">User Prompt:</div>
        <pre>\${escapeHtml(turn.user_prompt)}</pre>
        \${turn.response ? \`
          <div class="divider">\${'\\u2500'.repeat(40)}</div>
          <div class="response-header">Response:</div>
          <div class="markdown-content">\${marked.parse(turn.response)}</div>
        \` : ''}
      \`;
    }

    function renderThinkingTab(turn) {
      if (!turn.thinking) {
        return '<div class="empty-state">No thinking available for this turn</div>';
      }
      return \`
        <div class="section-header magenta">Model Thinking:</div>
        <div class="markdown-content">\${marked.parse(turn.thinking)}</div>
      \`;
    }

    function renderToolsTab(turn) {
      const tools = turn.tool_invocations || [];
      if (tools.length === 0) {
        return '<div class="empty-state">No tool calls in this turn</div>';
      }

      const selectedTool = tools[selectedToolIndex];

      return \`
        <div class="section-header">Tool Calls (\${tools.length} total) - j/k to navigate, Enter to open subagent</div>
        <div class="tool-list">
          \${tools.map((tool, i) => {
            const isSelected = i === selectedToolIndex;
            const isOpenable = tool.tool_type?.Task?.subagent_turns?.length > 0;
            const toolName = getToolName(tool);
            const toolContext = getToolContext(tool);

            return \`
              <div class="tool-item \${isSelected ? 'selected' : ''} \${isOpenable ? 'openable' : ''}" data-index="\${i}">
                <span class="tool-number">[\${i + 1}]</span>
                <span class="tool-name \${isSelected ? 'selected' : ''} \${isOpenable ? 'openable' : ''}">\${escapeHtml(toolName)}</span>
                <span class="tool-context">\${escapeHtml(toolContext)}</span>
              </div>
            \`;
          }).join('')}
        </div>
        \${selectedTool ? \`
          <div class="tool-detail">
            <div class="tool-detail-header input">Input:</div>
            <div class="tool-detail-content"><pre>\${escapeHtml(selectedTool.input_display || '')}</pre></div>
            <div class="tool-detail-header output">Output:</div>
            <div class="tool-detail-content"><pre>\${escapeHtml(truncate(selectedTool.output_display || '', 2000))}</pre></div>
            \${selectedTool.tool_type?.Task?.subagent_turns?.length > 0 ? \`
              <div class="subagent-hint">Press Enter to view subagent conversation</div>
            \` : ''}
          </div>
        \` : ''}
      \`;
    }

    function renderDiffTab(turn) {
      const diffs = collectDiffs(turn);
      if (diffs.length === 0) {
        return '<div class="empty-state">No diffs available for this turn</div>';
      }

      return diffs.map(diff => \`
        <div class="diff-header">\${'\\u2500'.repeat(3)} \${escapeHtml(diff.path)} \${'\\u2500'.repeat(3)}</div>
        <pre>\${renderDiffContent(diff.content)}</pre>
      \`).join('');
    }

    function renderDiffContent(content) {
      return content.split('\\n').map(line => {
        if (line.startsWith('+') && !line.startsWith('+++')) {
          return '<span class="diff-add">' + escapeHtml(line) + '</span>';
        } else if (line.startsWith('-') && !line.startsWith('---')) {
          return '<span class="diff-remove">' + escapeHtml(line) + '</span>';
        } else if (line.startsWith('@@')) {
          return '<span class="diff-hunk">' + escapeHtml(line) + '</span>';
        }
        return escapeHtml(line);
      }).join('\\n');
    }

    function collectDiffs(turn) {
      const diffs = [];
      for (const tool of (turn.tool_invocations || [])) {
        const diff = getToolDiff(tool);
        if (diff) {
          diffs.push(diff);
        }
        // Collect from subagent turns
        const subturns = tool.tool_type?.Task?.subagent_turns || [];
        for (const subturn of subturns) {
          for (const subtool of (subturn.tool_invocations || [])) {
            const subDiff = getToolDiff(subtool);
            if (subDiff) {
              diffs.push({ ...subDiff, path: '[subagent] ' + subDiff.path });
            }
          }
        }
      }
      return diffs;
    }

    function getToolDiff(tool) {
      const type = tool.tool_type;
      if (type?.FileEdit) {
        return { path: type.FileEdit.path, content: type.FileEdit.diff || '' };
      }
      if (type?.FileWrite) {
        const lines = (type.FileWrite.content || '').split('\\n').map(l => '+' + l).join('\\n');
        return { path: type.FileWrite.path, content: '--- /dev/null\\n+++ ' + type.FileWrite.path + '\\n' + lines };
      }
      return null;
    }

    function getToolName(tool) {
      const type = tool.tool_type;
      if (type?.FileRead) return 'Read';
      if (type?.FileWrite) return 'Write';
      if (type?.FileEdit) return 'Edit';
      if (type?.Command) return 'Bash';
      if (type?.Search) return 'Search';
      if (type?.WebFetch) return 'WebFetch';
      if (type?.WebSearch) return 'WebSearch';
      if (type?.TodoUpdate) return 'TodoWrite';
      if (type?.Task) {
        const t = type.Task;
        const subType = t.subagent_type || 'Task';
        const turnCount = t.subagent_turns?.length || 0;
        return turnCount > 0 ? \`\${subType} (\${turnCount} turns) \\u23CE\` : subType;
      }
      if (type?.Other) return type.Other.name;
      return 'Unknown';
    }

    function getToolContext(tool) {
      const type = tool.tool_type;
      if (type?.FileRead) return type.FileRead.path.split('/').pop();
      if (type?.FileWrite) return type.FileWrite.path.split('/').pop();
      if (type?.FileEdit) return type.FileEdit.path.split('/').pop();
      if (type?.Command) return truncate(type.Command.command, 50);
      if (type?.Search) return truncate(type.Search.pattern, 50);
      if (type?.WebFetch) return truncate(type.WebFetch.url, 50);
      if (type?.WebSearch) return truncate(type.WebSearch.query, 50);
      if (type?.Task) return truncate(type.Task.description, 40);
      return '';
    }

    function tryOpenSubagent() {
      const ctx = getCurrentContext();
      const turn = ctx.turns[selectedTurnIndex];
      if (!turn) return;

      const tool = turn.tool_invocations?.[selectedToolIndex];
      if (!tool) return;

      const subturns = tool.tool_type?.Task?.subagent_turns;
      if (!subturns || subturns.length === 0) return;

      const title = tool.tool_type.Task.subagent_type || tool.tool_type.Task.description || 'Subagent';
      contextStack.push({ title, turns: subturns });
      selectedTurnIndex = 0;
      selectedToolIndex = 0;
      render();
    }

    function goBack() {
      if (contextStack.length > 1) {
        contextStack.pop();
        selectedTurnIndex = 0;
        selectedToolIndex = 0;
        render();
      }
    }

    function escapeHtml(str) {
      if (!str) return '';
      return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }

    function truncate(str, maxLen) {
      if (!str) return '';
      str = str.replace(/\\n/g, ' ');
      return str.length > maxLen ? str.slice(0, maxLen - 1) + '\\u2026' : str;
    }

    // Keyboard navigation
    document.addEventListener('keydown', (e) => {
      const ctx = getCurrentContext();
      if (!ctx) return;

      switch (e.key) {
        case 'ArrowUp':
          e.preventDefault();
          if (selectedTurnIndex > 0) {
            selectedTurnIndex--;
            selectedToolIndex = 0;
            render();
          }
          break;
        case 'ArrowDown':
          e.preventDefault();
          if (selectedTurnIndex < ctx.turns.length - 1) {
            selectedTurnIndex++;
            selectedToolIndex = 0;
            render();
          }
          break;
        case 'ArrowLeft':
          e.preventDefault();
          const tabs = ['prompt', 'thinking', 'tools', 'diff'];
          const currentIndex = tabs.indexOf(activeTab);
          activeTab = tabs[(currentIndex - 1 + tabs.length) % tabs.length];
          render();
          break;
        case 'ArrowRight':
        case 'Tab':
          e.preventDefault();
          const tabsList = ['prompt', 'thinking', 'tools', 'diff'];
          const currIndex = tabsList.indexOf(activeTab);
          activeTab = tabsList[(currIndex + 1) % tabsList.length];
          render();
          break;
        case 'j':
          e.preventDefault();
          if (activeTab === 'tools') {
            const turn = ctx.turns[selectedTurnIndex];
            const tools = turn?.tool_invocations || [];
            if (selectedToolIndex < tools.length - 1) {
              selectedToolIndex++;
              render();
            }
          } else {
            document.querySelector('.content')?.scrollBy(0, 50);
          }
          break;
        case 'k':
          e.preventDefault();
          if (activeTab === 'tools') {
            if (selectedToolIndex > 0) {
              selectedToolIndex--;
              render();
            }
          } else {
            document.querySelector('.content')?.scrollBy(0, -50);
          }
          break;
        case 'g':
          e.preventDefault();
          document.querySelector('.content')?.scrollTo(0, 0);
          break;
        case 'G':
          e.preventDefault();
          const content = document.querySelector('.content');
          if (content) content.scrollTo(0, content.scrollHeight);
          break;
        case 'Enter':
          e.preventDefault();
          if (activeTab === 'tools') {
            tryOpenSubagent();
          }
          break;
        case 'Escape':
          e.preventDefault();
          goBack();
          break;
      }
    });

    // Load the session
    loadSession();
  </script>
</body>
</html>`;
}
