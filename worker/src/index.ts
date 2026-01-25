import { nanoid } from 'nanoid';

export interface Env {
  SESSIONS: R2Bucket;
  CORS_ORIGIN: string;
}

// Rate limiting: Track uploads per IP (in-memory, resets on worker restart)
const uploadCounts = new Map<string, { count: number; resetAt: number }>();
const RATE_LIMIT = 10; // uploads per hour
const RATE_WINDOW = 60 * 60 * 1000; // 1 hour in ms

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

function corsHeaders(env: Env): HeadersInit {
  return {
    'Access-Control-Allow-Origin': env.CORS_ORIGIN,
    'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
    'Access-Control-Allow-Headers': 'Content-Type, Content-Encoding',
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;

    // Handle CORS preflight
    if (request.method === 'OPTIONS') {
      return new Response(null, {
        status: 204,
        headers: corsHeaders(env),
      });
    }

    // API Routes
    if (path === '/api/sessions' && request.method === 'POST') {
      return handleUpload(request, env);
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

    // Redirect root to GitHub
    if (path === '/') {
      return Response.redirect('https://github.com/dzhanguzin/vibereview', 302);
    }

    return new Response('Not Found', { status: 404 });
  },
};

async function handleUpload(request: Request, env: Env): Promise<Response> {
  // Check rate limit
  const ip = request.headers.get('CF-Connecting-IP') || 'unknown';
  if (!checkRateLimit(ip)) {
    return new Response(JSON.stringify({ error: 'Rate limit exceeded. Try again later.' }), {
      status: 429,
      headers: {
        ...corsHeaders(env),
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
        ...corsHeaders(env),
        'Content-Type': 'application/json',
      },
    });
  }

  // Limit size to 10MB
  if (body.byteLength > 10 * 1024 * 1024) {
    return new Response(JSON.stringify({ error: 'Session too large (max 10MB)' }), {
      status: 413,
      headers: {
        ...corsHeaders(env),
        'Content-Type': 'application/json',
      },
    });
  }

  // Generate ID (12 chars, URL-safe)
  const id = nanoid(12);

  // Store in R2
  await env.SESSIONS.put(id, body, {
    httpMetadata: {
      contentType: 'application/octet-stream',
      contentEncoding: 'zstd',
    },
    customMetadata: {
      uploadedAt: new Date().toISOString(),
      ip: ip,
    },
  });

  const baseUrl = new URL(request.url).origin;
  const shareUrl = `${baseUrl}/s/${id}`;

  return new Response(
    JSON.stringify({
      id,
      url: shareUrl,
    }),
    {
      status: 201,
      headers: {
        ...corsHeaders(env),
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
        ...corsHeaders(env),
        'Content-Type': 'application/json',
      },
    });
  }

  const object = await env.SESSIONS.get(id);
  if (!object) {
    return new Response(JSON.stringify({ error: 'Session not found' }), {
      status: 404,
      headers: {
        ...corsHeaders(env),
        'Content-Type': 'application/json',
      },
    });
  }

  return new Response(object.body, {
    headers: {
      ...corsHeaders(env),
      'Content-Type': 'application/octet-stream',
      'Cache-Control': 'public, max-age=31536000, immutable',
      'X-Robots-Tag': 'noindex',
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
  </style>
</head>
<body>
  <div id="app" class="loading">Loading session...</div>

  <script type="module">
    import { decompress } from 'https://esm.sh/fzstd@0.1.1';

    const SESSION_ID = '${sessionId}';
    const API_URL = '/api/sessions/' + SESSION_ID;

    let session = null;
    let turns = [];
    let contextStack = [];
    let selectedTurnIndex = 0;
    let selectedToolIndex = 0;
    let activeTab = 'prompt';

    async function loadSession() {
      try {
        const response = await fetch(API_URL);
        if (!response.ok) {
          throw new Error('Session not found');
        }

        const compressed = await response.arrayBuffer();
        const decompressed = decompress(new Uint8Array(compressed));
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
          <pre>\${escapeHtml(turn.response)}</pre>
        \` : ''}
      \`;
    }

    function renderThinkingTab(turn) {
      if (!turn.thinking) {
        return '<div class="empty-state">No thinking available for this turn</div>';
      }
      return \`
        <div class="section-header magenta">Model Thinking:</div>
        <pre>\${escapeHtml(turn.thinking)}</pre>
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
