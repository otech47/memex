# recall, for coding agents

recall finds a past coding-agent session on this machine from a description and prints its id and resume command. It reads Claude Code, Codex, Kimi Code and Cursor transcripts in place. One Rust binary, no runtime dependencies.

## Install

Check first: `command -v recall`. If it is missing:

```
cargo build --release
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/recall" ~/.local/bin/recall
```

If `cargo` is missing, install it with `curl https://sh.rustup.rs -sSf | sh -s -- -y` and reopen the shell. The first run builds the index at `~/.agents/recall/index.jsonl`, which takes a few seconds on a large machine. Every later run refreshes it in well under a second.

## Use

Ranking only, no model call. This is the default and the fastest:

```
recall "<the user's description, verbatim>" -x <your own session id> -j
```

Always pass `-x` with your own session id. The session asking the question contains the description and would otherwise match itself. Pass `-j` when you are going to read the result. It gives one object per hit with id, harness, project, dates, title, score, matched terms, first prompt and up to 15 prompts.

Read the prompt lists, not the titles. A title comes from the first prompt and often names only how a session started. If the top hit's prompts do not match what the user described, look at the next few before answering.

Narrow when the user gives you a handle:

```
-f claude|codex|kimi|cursor     one harness
-p <text>                        project directory or path substring
-s YYYY-MM-DD   -u YYYY-MM-DD    date window
-n <count>                       more or fewer hits
```

More on one session, and the command that reopens it:

```
recall show <id-prefix>
recall resume <id-prefix>
```

Search the raw transcripts when the words you have are from the agent's output rather than the user's prompts, for example an error string or a file name:

```
recall grep '<regex>' -f claude -n 5
```

Agent mode, which spends one model call on the pick, is for the human at the terminal (`recall agent "..."`). You are already a model. Run the plain search and judge the prompt lists yourself.

## Report

Give the user the harness, date, project, title, id and the resume command, and one sentence on why that session matches. If nothing plausible matches, say so and show the top three.

`recall -h` prints the full help.
