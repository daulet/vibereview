# promptui-share Worker

Cloudflare Worker for session sharing. Handles upload, storage (R2), and web viewer.

## Setup

1. Install dependencies:
   ```bash
   npm install
   ```

2. Create the R2 bucket:
   ```bash
   wrangler r2 bucket create promptui-sessions
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
| `/api/sessions` | POST | Upload compressed session (zstd). Returns `{id, url}` |
| `/api/sessions/:id` | GET | Get compressed session |
| `/s/:id` | GET | Web viewer |

## Rate Limits

- 10 uploads per hour per IP
- Max session size: 10MB
