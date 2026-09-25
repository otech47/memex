# memex

Describe a past coding-agent session from memory and get back its id and resume command. Reads every Claude Code, Codex, Kimi Code and Cursor transcript on the machine. One Rust binary, no runtime dependencies.

```
memex "the one where I made my claude skills and hooks work in codex, then kimi, then cursor"
memex agent "..."                    same, then one model call picks from the top ten (claude sonnet by default)
memex -r "..."                       run the resume command of the first hit
memex agent -a codex -m gpt-5 "..."  a different agent and model do the picking
```

The bare form is deterministic keyword ranking and finishes in well under a second. The `agent` form adds one model call, 3 to 10 seconds with sonnet.

Every option has a short and a long form:

```
-r --resume    -a --agent    -m --model    -n --limit    -f --from     -p --project
-s --since     -u --until    -x --exclude  -j --json     -A --all      -F --full
-N --no-refresh   -T --threads   -h --help
```

Any agent can use the same binary instead of reading the transcript directories by hand:

```
memex codex hooks skills -s 2026-09-01 -j
memex grep 'AGENTS\.md.*symlink' -f claude
memex show a122183c
memex resume a122183c
memex index
memex stats
```

`memex -h` prints the full help.

## Install

```
cargo build --release
ln -s "$PWD/target/release/memex" ~/.local/bin/memex
```

The index lives at `~/.agents/memex/index.jsonl` (`MEMEX_INDEX_DIR` moves it). It refreshes on every command, reparsing only transcripts whose mtime or size changed. `MEMEX_AGENT` and `MEMEX_MODEL` set the defaults for `agent` mode.

## What the index holds per session

harness, id, cwd and project, git branch, start and end time, title (Claude ai-title or custom-title, Codex thread name, Cursor title), compaction summaries, every real user prompt (injected system text, tool results, slash commands and hook output are stripped), file paths the agent touched, turn count, and the resume command.

| harness | transcript | resume |
|---|---|---|
| Claude Code | `~/.claude/projects/<slug>/<id>.jsonl` (subagent `agent-*.jsonl` skipped) | `claude --resume <id>` |
| Codex | `~/.codex/sessions/Y/M/D/rollout-*.jsonl` (only `thread_source: user`) plus `session_index.jsonl` for names | `codex resume <id>` |
| Kimi Code | `~/.kimi-code/sessions/wd_*/session_*/agents/main/wire.jsonl` plus `state.json` | `kimi --session <id>` |
| Cursor | `~/.cursor/chats/<ws>/<id>/meta.json` and `prompt_history.json` (the store.db is encrypted) plus `~/.cursor/sessions/<id>.jsonl` for tool paths | `agent --resume <id>` |

## Ranking

Terms are lowercased, stopwords dropped, plurals and -ed/-ing folded. Score per session is the sum over matched terms of idf times a field weight (title 4, project 3, first prompt 2.5, branch 2, summary 1.5, paths 1.5, later prompts 1) times log(1 + count), scaled by the fraction of query terms present, with a 15 percent boost decaying over 90 days. Same query, same index, same result every time.

## Where the model is still needed

The dictated description is the only non-deterministic input. On the test case (a session that started as a subscription comparison and turned into porting the whole agent harness to three more CLIs, described from memory in four sentences) the ranking alone put the right session first or second out of 1568, depending on the phrasing. Two things ranking cannot do:

- Know that the session asking the question is not the answer. `-x` takes the asking session's id.
- Tell a session that turned into the thing you describe apart from one whose title names it. Titles come from the first prompt. The model picks correctly only when it sees each candidate's prompt list, which is what `agent` mode sends it: the top ten candidates with up to twelve prompts each, about 4500 tokens, one call.

Sonnet answers in 3 to 10 seconds and haiku in 15 to 25 on the same account, so sonnet is the default. Claude runs with hooks, MCP and settings disabled. Codex, Kimi and Cursor each need 25 to 40 seconds just to start in print mode, so they are options, not defaults. `-a local` uses an ollama server on port 11434.

## Why Rust

The tool was first built twice, in stdlib Python and in Rust, with byte-identical output, to measure the difference. Rust reparsed all 1568 transcripts in 0.3 seconds against 5.2, and a regex grep across every transcript in 0.2 seconds against 22. Only the Rust engine is kept.
