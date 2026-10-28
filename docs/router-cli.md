# Use Chalk Router with coding clients

[Documentation index](README.md) · [Authentication](../README.md#authentication)

Configure Codex, Claude Code, OpenCode, VS Code, and Cursor to route LLM
requests through Chalk Router.

These commands belong to the separately distributed Chalk CLI, whose source
is outside this repository. Check your installed `chalk router --help` if its
available commands differ. Admin operations also depend on the backend's
authorization policy; see the [pending authentication changes](authentication-transition.md).

**Contents:** [Quick start](#quick-start) · [Supported clients](#supported-clients) ·
[Model selection](#model-selection) · [Codex](#codex) ·
[Claude Code](#claude-code) · [OpenCode](#opencode) · [VS Code](#vs-code) ·
[Cursor](#cursor) · [Verification](#verification)

## Quick start

### 1. Install the Chalk CLI

If the `chalk` command is not already installed:

```sh
curl -s -L https://api.chalk.ai/install.sh | sh
```

### 2. Sign in and select an environment

Run these commands from the Chalk project directory. Replace
`<environment>` with the environment you want to use.

```sh
cd /path/to/your-project

chalk login
chalk environment <environment>
```

### 3. Check the router

```sh
chalk healthcheck llm_router
chalk router model list
chalk router status
chalk router doctor
```

### 4. Connect a client

Choose one of the supported clients below. For example:

```sh
chalk router codex on
chalk router codex status
```

Restart or reload the client after enabling routing, then run the final
verification checks.

## Supported clients

| Client | Enable | Authentication | After enabling |
| --- | --- | --- | --- |
| [Codex](#codex) | `chalk router codex on` | Refreshable Chalk login | Restart Codex |
| [Claude Code](#claude-code) | `chalk router claude on` | Refreshable Chalk login | Restart Claude Code |
| [OpenCode](#opencode) | `chalk router opencode on` | Refreshable Chalk login | Restart OpenCode |
| [VS Code](#vs-code) | `chalk router vscode on --router-api-key-stdin` | Revocable Router API key | Reload the VS Code window |
| [Cursor](#cursor) | `chalk router cursor on` | Refreshable Chalk token | Follow the printed instructions |

Every client has a `status` command and an `off` command. The refreshable Chalk
login is preferred for Codex, Claude Code, and OpenCode. VS Code uses a static
Router API key because its Custom Endpoints cannot run the refreshable
credential helper. Cursor requires manual configuration because the CLI does
not modify Cursor's internal database or undocumented configuration files.

## Model selection

When `--model` is omitted, the CLI:

1. Reuses the existing compatible model.
2. Automatically selects the only compatible model.
3. Opens the searchable model catalog when multiple compatible models are
   available.

Use the arrow keys to navigate the catalog and press Enter to select a model.
Replace `<model-id>` with an ID from `chalk router model list` when selecting a
model explicitly.

Claude Code also supports independent aliases for its model slots; see the
[Claude Code](#claude-code) section.

## Codex

Codex uses the preferred refreshable Chalk login.

```sh
chalk router codex on
chalk router codex status
```

Quit and restart Codex after enabling routing.

To disable routing:

```sh
chalk router codex off
```

## Claude Code

Claude Code uses the preferred refreshable Chalk login.

```sh
chalk router claude on
chalk router claude status
```

Optional model aliases can be configured independently:

```sh
chalk router claude on \
  --opus <model-id> \
  --sonnet <model-id> \
  --haiku <model-id> \
  --fable <model-id> \
  --subagent <model-id>
```

Quit and restart Claude Code after enabling routing.

To disable routing:

```sh
chalk router claude off
```

## OpenCode

OpenCode uses the preferred refreshable Chalk login.

```sh
chalk router opencode on
chalk router opencode status
```

Quit and restart OpenCode after enabling routing.

To disable routing:

```sh
chalk router opencode off
```

## VS Code

VS Code Custom Endpoints cannot run the refreshable Chalk credential helper.
The recommended setup uses a revocable Router API key entered through a hidden
prompt.

Creating a usage pool is optional:

```sh
chalk router usage-pool create --name vscode
chalk router usage-pool list
```

Create a Router API key. The raw key is displayed only once.

```sh
chalk router api-key create --description "VS Code"
```

If a usage pool was created, attach the key while creating it instead. Replace
`<pool-id>` with the pool ID from `chalk router usage-pool list`:

```sh
chalk router api-key create --description "VS Code" --usage-pool <pool-id>
```

Configure VS Code and paste the newly created key into the hidden prompt:

```sh
chalk router vscode on --router-api-key-stdin
chalk router vscode status
```

Reload the VS Code window after enabling routing.

To disable routing:

```sh
chalk router vscode off
```

To revoke the static Router API key:

```sh
chalk router api-key list
chalk router api-key revoke <key-hash>
```

To delete an unused usage pool:

```sh
chalk router usage-pool list
chalk router usage-pool delete <pool-id>
```

## Cursor

Cursor configuration is manual because the CLI does not modify Cursor's
internal database or undocumented configuration files.

```sh
chalk router cursor on
chalk router cursor status
```

Follow the printed instructions under **Cursor Settings → Models → OpenAI**.
Obtain the refreshable Chalk token from the initialized project directory with:

```sh
chalk token get
```

To display the cleanup instructions:

```sh
chalk router cursor off
```

## Verification

After enabling or changing a client configuration, run:

```sh
chalk router status
chalk router doctor
```

For additional diagnostic checks:

```sh
chalk router doctor --verbose
```
