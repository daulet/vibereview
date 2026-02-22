# VibeReview Worker

Cloudflare Worker for session sharing. Handles upload, storage (R2), and web viewer.
New shares are encrypted client-side by default (with optional public mode);
the worker stores whatever payload the client uploads.
Uploads and upload listing are authenticated via GitHub user identity.
Public-mode uploads are additionally indexed for the website home page.

## Setup

1. Install dependencies:
   ```bash
   npm install
   ```

2. Create the R2 bucket:
   ```bash
   wrangler r2 bucket create vibereview-sessions
   ```

3. Deploy:
   ```bash
   npm run deploy
   ```

4. Configure vars:
   - `GITHUB_CLIENT_ID`: OAuth app client ID used by CLI device flow login.
   - Allowed browser origin (CORS) is hardcoded to `https://vibereview.trustme.workers.dev`.

Example:

```bash
wrangler deploy --var GITHUB_CLIENT_ID=<your_client_id>
```

## Development

```bash
npm run dev
```

## API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/auth/github/client-id` | GET | Returns configured GitHub OAuth client ID |
| `/api/sessions` | POST | Upload payload (encrypted by default, public optional). Requires `Authorization: Bearer <github_token>`. Returns `{id, url}` |
| `/api/uploads` | GET | List uploads for authenticated GitHub user (`Authorization` required) |
| `/api/public-uploads` | GET | List recent public uploads for website home page |
| `/api/sessions/:id` | GET | Get stored payload |
| `/s/:id` | GET | Web viewer (for encrypted links include key fragment, e.g. `#k=...`) |
| `/` | GET | Home page listing recent public uploads |

## Rate Limits

- 10 uploads per hour per IP
- Max session size: 10MB
