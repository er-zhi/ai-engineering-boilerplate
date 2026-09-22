# Operator catalog

What this deployment is connected to. Both files are gitignored: they name live endpoints, they
differ per deployment, and one of them carries `${SECRET}` placeholders resolved from the
environment.

| File | What it says |
|---|---|
| `declarative-tools.json` | Whole sources, as rows: url, argument schema, what each covers |
| `built-in-tools.json` | For the tools compiled into Tool: what each covers, and whether it is in circulation |

`compose.yaml` mounts this directory into the Tool container read-only and sets both paths
unconditionally. A missing file is not an error — Tool logs it and starts with what the binary
declares — so a fresh clone runs without either, and adding one is a file edit and a restart.

`declarative-tools.example.json` and `built-in-tools.example.json` are tracked and show the shape of
each. Copy one, drop the `.example`, and edit. The shapes are also documented in
[the Tool service README](../services/tool/README.md#declarative-tools).

Without either file the stack runs with what the binary declares: the built-in tools, no declarative
rows. Every golden case that expects a connected source then declines, which is correct behaviour
and a useless test run — connect a catalog before judging one.
Every `${NAME}` a row uses must also reach the container through `compose.yaml`'s `environment`
block: a source whose secret is unset is refused, and that refusal drops the whole file.
