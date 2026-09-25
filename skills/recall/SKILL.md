---
name: recall
description: >-
  Find a past coding-agent session (Claude Code, Codex, Kimi Code, Cursor) from a description and print its id and resume command. Triggers on: "find the session where", "which session did I", "resume the one about", "that conversation from last week", "where did I do X before". Installs recall from github.com/otech47/recall if it is missing.
---

# recall

recall ranks every coding-agent session on this machine against a description and prints the id and the command that reopens it. Deterministic, no model call, under a second after the first index build.

## Install if missing

```
command -v recall || (
  git clone https://github.com/otech47/recall ~/.agents/recall-src &&
  cd ~/.agents/recall-src && cargo build --release &&
  mkdir -p ~/.local/bin && ln -sf "$PWD/target/release/recall" ~/.local/bin/recall
)
```

If `cargo` is missing, install it with `curl https://sh.rustup.rs -sSf | sh -s -- -y` first.

## Find the session

```
recall "<the user's description, verbatim>" -x <your own session id> -j
```

- `-x` with your own session id is required. The session asking the question contains the description and matches itself otherwise.
- Read the `prompts` list of each hit, not the `title`. Titles come from the first prompt and often name only how a session started.
- Narrow with `-f <harness>`, `-p <project>`, `-s <since>`, `-u <until>`, `-n <count>` when the user gives you a handle.
- `recall show <id>` prints one session in full. `recall grep '<regex>'` scans the raw transcripts when the words come from agent output rather than user prompts.

Do not use `recall agent`. That mode spends a model call on the pick for a human at the terminal. You are the model here.

## Answer

Harness, date, project, title, id, the resume command, and one sentence on why that session matches. If nothing plausible matches, say so and show the top three.
