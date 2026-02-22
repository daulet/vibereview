# VibeReview Worker

Cloudflare Worker for session sharing. Handles upload, storage (R2), and web viewer.
New shares are encrypted client-side; the worker stores ciphertext only.

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

## Development

```bash
npm run dev
```

## API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/sessions` | POST | Upload encrypted payload (ciphertext blob). Returns `{id, url}` |
| `/api/sessions/:id` | GET | Get stored payload (encrypted for new shares) |
| `/s/:id` | GET | Web viewer (expects key in URL fragment, e.g. `#k=...`) |

## Rate Limits

- 10 uploads per hour per IP
- Max session size: 10MB
