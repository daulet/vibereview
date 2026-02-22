# Vibe Review

Harness for your AI harness. Browse Codex, Claude Code turns/tools/diffs, resume locally or share.

## Install

```
> brew install daulet/tap/vibereview
```

## Features

- Browse Codex and Claude Code sessions in terminal UI;
- Dig into tool calls, subagents, thinking etc;
- Resume option (currently doesn't restore git state);
- Share session as a file or as an encrypted cloud link;
- Import shared session from a file or cloud share URL;
- Login with GitHub from CLI for authenticated cloud uploads;
- List your cloud uploads from the service (`vibereview uploads`);

Cloud links default to encrypted and include a decryption key in URL fragment (`#k=...`).
You can optionally switch to a public (unencrypted) cloud link in the share dialog.
Keep the full URL to share or import via:
`vibereview import "<share-url>"`.

## Cloud Auth

Cloud uploads require GitHub login:

```bash
vibereview login
```

List uploads associated with your GitHub identity:

```bash
vibereview uploads
```

Website home page (`/`) lists recent public uploads.
