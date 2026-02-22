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

Cloud links include a decryption key in URL fragment (`#k=...`). Keep the full URL to share or import via:
`vibereview import "<share-url>"`.
