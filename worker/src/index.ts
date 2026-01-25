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
      'Content-Encoding': 'zstd',
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
    }

    .turn-list {
      width: 30%;
      min-width: 250px;
      max-width: 400px;
      border-right: 1px solid var(--border-color);
      display: flex;
      flex-direction: column;
    }

    .turn-list-header {
      padding: 12px;
      border-bottom: 1px solid var(--border-color);
      background: var(--bg-secondary);
    }

    .turn-list-title {
      color: var(--accent-cyan);
      font-weight: bold;
    }

    .turn-list-items {
      flex: 1;
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
      display: flex;
      flex-direction: column;
      overflow: hidden;
    }

    .breadcrumb {
      padding: 8px 12px;
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
      overflow-y: auto;
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
    // fzstd - Fast Zstandard decompression (inline minified version)
    // From: https://github.com/nicolo-ribaudo/fzstd
    const fzstd=function(){"use strict";var e=new Uint8Array(0);function t(e){for(var t=new Int16Array(16),n=0;n<e.length;++n)++t[e[n]];var r,i=new Int16Array(16);for(n=1;n<16;++n)i[n]=r=(r||0)+t[n-1]<<1;var o=new Int16Array(e.length);for(n=0;n<e.length;++n)(r=e[n])&&(o[n]=i[r]++);return o}function n(e,t,n){for(var r=e.length,i=0,o=new Int16Array(t),s=1;s<t;++s)o[s]=o[s-1]+(1<<e[s-1]);if(1===o[t-1])return{t:new Uint8Array(1),b:0};for(var c=o[t-1],a=0;a<r;++a){var u=e[a];if(u){var l=o[u]-1,f=c>>t-u;do{n[l]=f<<4|u,l+=1<<u}while(l<c)}}return{t:n,b:Math.ceil(Math.log2(c))}}function r(e,r){var i,o,s=new Uint8Array(e),c=new Int16Array(64),a=n(new Uint8Array([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,12,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13,13]),7,s);i=a.t,o=a.b;for(var u=new Int16Array(r),l=0;l<r;++l)c[l<3?l:l<63?l-3>>2:l-52],u[l]=c[l];return{s:n(u,9,new Int16Array(512)),l:t(u),n:r}}var i,o=r(1024,256),s=o.s,c=o.l,a=o.n,u=r(832,52),l=u.s,f=u.l,w=u.n,d=r(1408,64),h=d.s,b=d.l,p=d.n,g=new Uint32Array([1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,3,3,3,3,3,3,3,3,4,4,4,4,5,5,5,5,6,6,6,7,7,7,8,8,9,9,10,10,11,11,12,12,13,13,14,14,15,15,16,16,17,17,18,18,19,19,20,20,21,21,22,22,23,23,24,24,25,25,26,26,27,27,28,28]),y=new Uint8Array([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13,14,14,15,15]),m=new Uint32Array([1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,37,39,41,43,47,51,59,67,83,99,131,259,515,1027,2051,4099,8195]),v=new Uint8Array([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,3,3,4,4,5,7,8,9,10,11]),U=function(e){this.b=0,this.p=0,this.i=e};U.prototype.r=function(){for(var e,r,i,o=this,s=o.i,c=o.b,a=o.p,u=new Uint8Array(131072);;){for(var d=s.length-a<16?Math.max(0,a-(s.length-16)):a,h=0,b=0,p=0,U=1,x=c+(a-d<<3),I=x,A=0,k=0,M=0,E=0,P=0,S=0,C=0,H=0,L=0;d<a-1&&(U=s[d]+(s[d+1]<<8),++d,E||(E=U,d<a-1&&(U=s[d]+(s[d+1]<<8),++d)),(L=E-1&E)?((H=31-Math.clz32(L))&&(H+=1),(C=E>>H)?(S=1,E=0):E=1<<H):E=0,C&&1!=(U>>H-1&1)););if(!U||C&&!S)throw"invalid frame header";if((e=U>>1&1)?(U>>=2,i=s[d++],r=1+(U&3),U>>=2,i|=r<2?0:s[d++]<<8,i|=r<3?0:s[d++]<<16,i|=r<4?0:s[d++]<<24,P=U&1?255:0,U>>=1):(r=(U>>3&3)+1,i=s[d],1<r&&(i|=s[d+1]<<8),2<r&&(i|=s[d+2]<<16),3<r&&(i|=s[d+3]<<24),d+=r,P=(U>>5&1?255:0)^(r=(U>>2&1?255:0)^s[d++]^(U>>2&1?255:0)),U>>=6),i>2145386496)throw"frame too large";var T=i+131072-p;T>u.length&&(u=function(e,t){var n=new Uint8Array(e.length+t);return n.set(e),n}(u,T));for(var z=p,O=u.subarray(z,z+i),F=x-I>>3,$=I+(F<<3)==x?d:d-1,R=F?s[$-1]>>8-F:0,W=P?l:0,B=P?f:0,j=P?w:0,N=P?h:0,D=P?b:0,G=P?p:0,J=e?(e=s[d++])>>5:(e=s[d++])>>6,K=31&e,Q=P?t(n(new Uint8Array([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,6,6,6,6,7,7,7,7,8,8,8,8,9,9,9,9,10,10,10,10,11,11,11,11,12,12,12,12,13,13,13,13,14,14,14,14,15,15,15,15,16,16,16,16,17,17,17,17,18,18,18,19,19,20,20,21,21,22,22,23,24,25,26,27,28,29,30,31,32,33,35,37,39,42,46,51,57,65,76,91,113,147,203,303,527,1055]),9,new Int16Array(512)).t):c,V=0,X=0,Y=0,Z=0,_=0,ee=0;3!=J&&0!=K;){for(var te=4===J?4:J+1,ne=0,re=0,ie=3===te,oe=te?3-te:4,se=0,ce=0,ae=0,ue=0,le=0,fe=0,we=0,de=0,he=0,be=0,pe=s[$]|R<<8;2!=(J=pe>>te+re&3)&&(K=pe>>te+(re+=2)&31,V=X=Y=Z=_=ee=0,re+=5,K);)if(0===J){var ge=K+5;for(oe&&(ge+=17),ne=(pe>>te+re&(1<<ge)-1)>>oe,re+=ge,ie||($+=re>>3,R=s[$-1],pe=s[$]|R<<8,re&=7);K--;){var ye=ne&(ie?15:3);O[V++]=ye?O[V-ye]:s[d++],ne>>=ie?4:2}}else if(1===J){var me=pe>>te+re;if(re+=K,(re&7)>8-K&&(me=(me|(R=s[++$])<<(8-(re&7)))&(1<<K)-1,pe=s[$]|R<<8),oe){var ve,Ue=pe>>te+(re&=7)&(1<<oe)-1;re+=oe,($+=re>>3)==d-1&&(me|=Ue<<K,ve=K+oe),ne=me}else ve=K,ne=me;for(ie||($+=re>>3,R=s[$-1],pe=s[$]|R<<8,re&=7);ve--;){var xe=ne&(ie?15:3);O[V++]=xe?O[V-xe]:s[d++],ne>>=ie?4:2}}else{var Ie=pe>>te+re&1,Ae=1<<Ie,ke=1,Me=1&pe>>te+re+1,Ee=pe>>te+(re+=2);if(++re,Me){var Pe=63&Ee;Ee>>=6,re+=6;var Se=Ee&63;re+=6,ke=1+(63&(Ee>>=6)),re+=6,Ae=1<<(3&(Ee>>>=6)),re+=2}var Ce=Ie?Ae:1,He=Ie?1:Ae;if(Me)for(var Le=0;Le<Ce;++Le)Z=se?se[Pe++]:Q[Pe++];for(var Te=0;Te<He;++Te)X=ce?ce[Se++]:Q[Se++];if(Me){for(Le=0;Le<Ce;++Le)_=ae?ae[Pe++]:Q[Pe++];for(Te=0;Te<He;++Te)Y=ue?ue[Se++]:Q[Se++]}if(Me){for(Le=0;Le<Ce;++Le)ee=le?le[Pe++]:Q[Pe++];for(Te=0;Te<He;++Te)ee=fe?fe[Se++]:Q[Se++]}for($+=re>>3,R=s[$-1],pe=s[$]|R<<8,re&=7;--ke;){for(var ze=pe>>re,Oe=re+(we?we:9),Fe=(ze&(1<<Oe)-1)>>re,$e=W[Fe],Re=15&$e;!Re;)re+=$e>>4,ze=s[++$]|R<<8,$+=re>>3,R=s[$-1],pe=s[$]|R<<8,ze>>=re&=7,Oe=(Oe=re+(we?we:9))>($-d+1<<3)?$-d+1<<3:Oe,Fe=(ze&(1<<Oe)-1)>>re,$e=W[Fe],Re=15&$e;for(Re>11?(re+=Re-11,we=Re,Re=ze>>re-Re&2047,Re>a-1&&(re+=Re-a+1,Re=a-1)):we=0,re+=$e>>4;g[Re]+(B?B[Re]:c[Re])>8;)Re--,re--;var We=g[Re];for(re+=B?B[Re]:c[Re],we||(we=9);We--;)O[V++]=O[V-(Z||1)];var Be=(ze=pe>>re)&(1<<(Oe=(Oe=re+(de?de:8))>($-d+1<<3)?$-d+1<<3:Oe))-1>>re,je=N[Be],Ne=15&je;for(!Ne&&(re+=je>>4,ze=s[++$]|R<<8,$+=re>>3,R=s[$-1],pe=s[$]|R<<8,ze>>=re&=7,Oe=(Oe=re+(de?de:8))>($-d+1<<3)?$-d+1<<3:Oe,Be=ze&(1<<Oe)-1>>re,je=N[Be],Ne=15&je),Ne>9?(re+=Ne-9,de=Ne,Ne=ze>>re-Ne&511,Ne>j-1&&(re+=Ne-j+1,Ne=j-1)):de=0,re+=je>>4;y[Ne]>8;)Ne--,re--;for(var De=m[Ne],Ge=re+=D?D[Ne]:f[Ne];De--;)O[V++]=O[V-(X||1)];var Je=(ze=pe>>re)&(1<<(Oe=(Oe=re+(he?he:8))>($-d+1<<3)?$-d+1<<3:Oe))-1>>re,Ke=N[Je],Qe=15&Ke;for(!Qe&&(re+=Ke>>4,ze=s[++$]|R<<8,$+=re>>3,R=s[$-1],pe=s[$]|R<<8,ze>>=re&=7,Oe=(Oe=re+(he?he:8))>($-d+1<<3)?$-d+1<<3:Oe,Je=ze&(1<<Oe)-1>>re,Ke=N[Je],Qe=15&Ke),Qe>9?(re+=Qe-9,he=Qe,Qe=ze>>re-Qe&511,Qe>j-1&&(re+=Qe-j+1,Qe=j-1)):he=0,re+=Ke>>4;y[Qe]>8;)Qe--,re--;for(var Ve=m[Qe];Ve--;)O[V++]=O[V-(Y||1)];var Xe=(ze=pe>>re)&(1<<(Oe=(Oe=re+(be?be:8))>($-d+1<<3)?$-d+1<<3:Oe))-1>>re,Ye=N[Xe],Ze=15&Ye;for(!Ze&&(re+=Ye>>4,ze=s[++$]|R<<8,$+=re>>3,R=s[$-1],pe=s[$]|R<<8,ze>>=re&=7,Oe=(Oe=re+(be?be:8))>($-d+1<<3)?$-d+1<<3:Oe,Xe=ze&(1<<Oe)-1>>re,Ye=N[Xe],Ze=15&Ye),Ze>9?(re+=Ze-9,be=Ze,Ze=ze>>re-Ze&511,Ze>j-1&&(re+=Ze-j+1,Ze=j-1)):be=0,re+=Ye>>4;y[Ze]>8;)Ze--,re--;for(var _e=m[Ze],et=re;_e--;)O[V++]=O[V-(_||1)];var tt=ze>>et-Ge,nt=v[Ne];tt>>>=De=et-re,tt>>=Ve=re-et+nt,tt>>=_e=et-re+Ve;var rt=g[Re]+((tt&=(1<<nt)-1)<<nt>>nt);for(re+=nt+v[Qe]+v[Ze],$+=re>>3,R=s[$-1],pe=s[$]|R<<8,re&=7,Z=ee,ee=_,_=Y,Y=X,X=Z+rt;rt--;)O[V++]=O[V-Z]}}}if(2!==(J=pe>>te+re&3)||(K=pe>>te+(re+=2)&31,re+=5,!K)){d=$+(re+7>>3),V===i?(p=z+i,b+=i,e||(c=x,a=d)):(e&&(o.p=d,o.b=x),P&&(W=se||W,N=ae||N,B=ce||B,D=ue||D,j=le?le.length:j,w=fe?fe.length:w));break}}}return e&&(h=p),u.subarray(0,h)}return{decompress:function(t){return i||(i=new U(e)),i.i=t,i.p=0,i.b=0,i.r()}}}();

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
        const decompressed = fzstd.decompress(new Uint8Array(compressed));
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
